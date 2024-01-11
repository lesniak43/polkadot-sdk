// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Canonicalization window.
//! Maintains trees of block overlays and allows discarding trees/roots
//! The overlays are added in `insert` and removed in `canonicalize`.

use crate::{LOG_TARGET, LOG_TARGET_PIN};

use super::{to_meta_key, ChangeSet, CommitSet, DBValue, Error, Hash, MetaDb, StateDbError};
use codec::{Decode, Encode};
use log::trace;
use std::collections::{hash_map::Entry, HashMap, VecDeque};

const NON_CANONICAL_JOURNAL: &[u8] = b"noncanonical_journal";
pub(crate) const LAST_CANONICAL: &[u8] = b"last_canonical";
const OVERLAY_LEVEL_SPAN: &[u8] = b"noncanonical_overlay_span";
/// To decrease the number of database operations, the span of some overlay
/// level will be committed to the database iff index with this number belongs to it.
const OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN: u64 = 32;

/// See module documentation.
pub struct NonCanonicalOverlay<BlockHash: Hash, Key: Hash> {
	last_canonicalized: Option<(BlockHash, u64)>,
	levels: VecDeque<OverlayLevel<BlockHash, Key>>,
	parents: HashMap<BlockHash, BlockHash>,
	values: HashMap<Key, (u32, DBValue)>, // ref counted
	// would be deleted but kept around because block is pinned, ref counted.
	pinned: HashMap<BlockHash, u32>,
	pinned_insertions: HashMap<BlockHash, (Vec<Key>, u32)>,
	pinned_canonincalized: Vec<BlockHash>,
}

#[cfg_attr(test, derive(PartialEq, Debug))]
struct OverlayLevel<BlockHash: Hash, Key: Hash> {
	blocks: Vec<BlockOverlay<BlockHash, Key>>,
	used_indicies: u64, // Bitmask of available journal indices.
	span: u64,          // Largest index ever used + 1 (or 0 when None)
}

impl<BlockHash: Hash, Key: Hash> OverlayLevel<BlockHash, Key> {
	fn push(&mut self, overlay: BlockOverlay<BlockHash, Key>) {
		self.used_indicies |= 1 << overlay.journal_index;
		self.span = self.span.max(overlay.journal_index + 1);
		self.blocks.push(overlay)
	}

	fn available_index(&self) -> u64 {
		self.used_indicies.trailing_ones() as u64
	}

	fn span(&self) -> u64 {
		self.span
	}

	fn remove(&mut self, index: usize) -> BlockOverlay<BlockHash, Key> {
		self.used_indicies &= !(1 << self.blocks[index].journal_index);
		self.blocks.remove(index)
	}

	fn new_with_span(span: u64) -> OverlayLevel<BlockHash, Key> {
		OverlayLevel { blocks: Vec::new(), used_indicies: 0, span }
	}

	fn new() -> OverlayLevel<BlockHash, Key> {
		Self::new_with_span(0)
	}
}

#[derive(Encode, Decode)]
struct JournalRecord<BlockHash: Hash, Key: Hash> {
	hash: BlockHash,
	parent_hash: BlockHash,
	inserted: Vec<(Key, DBValue)>,
	deleted: Vec<Key>,
}

fn to_journal_key(block: u64, index: u64) -> Vec<u8> {
	to_meta_key(NON_CANONICAL_JOURNAL, &(block, index))
}

#[cfg_attr(test, derive(PartialEq, Debug))]
struct BlockOverlay<BlockHash: Hash, Key: Hash> {
	hash: BlockHash,
	journal_index: u64,
	journal_key: Vec<u8>,
	inserted: Vec<Key>,
	deleted: Vec<Key>,
}

fn insert_values<Key: Hash>(
	values: &mut HashMap<Key, (u32, DBValue)>,
	inserted: Vec<(Key, DBValue)>,
) {
	for (k, v) in inserted {
		debug_assert!(values.get(&k).map_or(true, |(_, value)| *value == v));
		let (ref mut counter, _) = values.entry(k).or_insert_with(|| (0, v));
		*counter += 1;
	}
}

fn discard_values<Key: Hash>(values: &mut HashMap<Key, (u32, DBValue)>, inserted: Vec<Key>) {
	for k in inserted {
		match values.entry(k) {
			Entry::Occupied(mut e) => {
				let (ref mut counter, _) = e.get_mut();
				*counter -= 1;
				if *counter == 0 {
					e.remove_entry();
				}
			},
			Entry::Vacant(_) => {
				debug_assert!(false, "Trying to discard missing value");
			},
		}
	}
}

fn discard_descendants<BlockHash: Hash, Key: Hash>(
	levels: &mut (&mut [OverlayLevel<BlockHash, Key>], &mut [OverlayLevel<BlockHash, Key>]),
	values: &mut HashMap<Key, (u32, DBValue)>,
	parents: &mut HashMap<BlockHash, BlockHash>,
	pinned: &HashMap<BlockHash, u32>,
	pinned_insertions: &mut HashMap<BlockHash, (Vec<Key>, u32)>,
	hash: &BlockHash,
) -> u32 {
	let (first, mut remainder) = if let Some((first, rest)) = levels.0.split_first_mut() {
		(Some(first), (rest, &mut *levels.1))
	} else if let Some((first, rest)) = levels.1.split_first_mut() {
		(Some(first), (&mut *levels.0, rest))
	} else {
		(None, (&mut *levels.0, &mut *levels.1))
	};
	let mut pinned_children = 0;
	if let Some(level) = first {
		while let Some(i) = level.blocks.iter().position(|overlay| {
			parents
				.get(&overlay.hash)
				.expect("there is a parent entry for each entry in levels; qed") ==
				hash
		}) {
			let overlay = level.remove(i);
			let mut num_pinned = discard_descendants(
				&mut remainder,
				values,
				parents,
				pinned,
				pinned_insertions,
				&overlay.hash,
			);
			if pinned.contains_key(&overlay.hash) {
				num_pinned += 1;
			}
			if num_pinned != 0 {
				// save to be discarded later.
				pinned_insertions.insert(overlay.hash.clone(), (overlay.inserted, num_pinned));
				pinned_children += num_pinned;
			} else {
				// discard immediately.
				parents.remove(&overlay.hash);
				discard_values(values, overlay.inserted);
			}
		}
	}
	pinned_children
}

impl<BlockHash: Hash, Key: Hash> NonCanonicalOverlay<BlockHash, Key> {
	/// Creates a new instance. Does not expect any metadata to be present in the DB.
	pub fn new<D: MetaDb>(db: &D) -> Result<NonCanonicalOverlay<BlockHash, Key>, Error<D::Error>> {
		let last_canonicalized =
			db.get_meta(&to_meta_key(LAST_CANONICAL, &())).map_err(Error::Db)?;
		let last_canonicalized = last_canonicalized
			.map(|buffer| <(BlockHash, u64)>::decode(&mut buffer.as_slice()))
			.transpose()?;
		let mut levels = VecDeque::new();
		let mut parents = HashMap::new();
		let mut values = HashMap::new();
		if let Some((ref hash, mut block)) = last_canonicalized {
			// read the journal
			trace!(
				target: LOG_TARGET,
				"Reading uncanonicalized journal. Last canonicalized #{} ({:?})",
				block,
				hash
			);
			let mut total: u64 = 0;
			block += 1;
			loop {
				let level_span = db
					.get_meta(&to_meta_key(OVERLAY_LEVEL_SPAN, &block))
					.map_err(Error::Db)?
					.map(|data| Decode::decode(&mut data.as_slice()))
					.transpose()?;
				let mut level = OverlayLevel::new_with_span(level_span.unwrap_or(0));
				for index in 0..level_span.unwrap_or(OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN) {
					let journal_key = to_journal_key(block, index);
					if let Some(record) = db.get_meta(&journal_key).map_err(Error::Db)? {
						let record: JournalRecord<BlockHash, Key> =
							Decode::decode(&mut record.as_slice())?;
						let inserted = record.inserted.iter().map(|(k, _)| k.clone()).collect();
						let overlay = BlockOverlay {
							hash: record.hash.clone(),
							journal_index: index,
							journal_key,
							inserted,
							deleted: record.deleted,
						};
						insert_values(&mut values, record.inserted);
						trace!(
							target: LOG_TARGET,
							"Uncanonicalized journal entry {}.{} ({:?}) ({} inserted, {} deleted)",
							block,
							index,
							record.hash,
							overlay.inserted.len(),
							overlay.deleted.len()
						);
						level.push(overlay);
						parents.insert(record.hash, record.parent_hash);
						total += 1;
					}
				}
				if level.blocks.is_empty() {
					break
				}
				levels.push_back(level);
				block += 1;
			}
			trace!(
				target: LOG_TARGET,
				"Finished reading uncanonicalized journal, {} entries",
				total
			);
		}
		Ok(NonCanonicalOverlay {
			last_canonicalized,
			levels,
			parents,
			pinned: Default::default(),
			pinned_insertions: Default::default(),
			values,
			pinned_canonincalized: Default::default(),
		})
	}

	/// Insert a new block into the overlay. If inserted on the second level or lower,
	/// expects parent to be present in the window.
	pub fn insert(
		&mut self,
		hash: &BlockHash,
		number: u64,
		parent_hash: &BlockHash,
		changeset: ChangeSet<Key>,
	) -> Result<CommitSet<Key>, StateDbError> {
		let mut commit = CommitSet::default();
		let front_block_number = self.front_block_number();
		if self.levels.is_empty() && self.last_canonicalized.is_none() && number > 0 {
			// assume that parent was canonicalized
			let last_canonicalized = (parent_hash.clone(), number - 1);
			commit
				.meta
				.inserted
				.push((to_meta_key(LAST_CANONICAL, &()), last_canonicalized.encode()));
			self.last_canonicalized = Some(last_canonicalized);
		} else if self.last_canonicalized.is_some() {
			if number < front_block_number || number > front_block_number + self.levels.len() as u64
			{
				trace!(
					target: LOG_TARGET,
					"Failed to insert block {}, current is {} .. {})",
					number,
					front_block_number,
					front_block_number + self.levels.len() as u64,
				);
				return Err(StateDbError::InvalidBlockNumber)
			}
			// check for valid parent if inserting on second level or higher
			if number == front_block_number {
				if !self
					.last_canonicalized
					.as_ref()
					.map_or(false, |&(ref h, n)| h == parent_hash && n == number - 1)
				{
					return Err(StateDbError::InvalidParent)
				}
			} else if !self.parents.contains_key(parent_hash) {
				return Err(StateDbError::InvalidParent)
			}
		}
		let level = if self.levels.is_empty() ||
			number == front_block_number + self.levels.len() as u64
		{
			self.levels.push_back(OverlayLevel::new());
			self.levels.back_mut().expect("can't be empty after insertion; qed")
		} else {
			self.levels.get_mut((number - front_block_number) as usize)
				.expect("number is [front_block_number .. front_block_number + levels.len()) is asserted in precondition; qed")
		};

		if level.blocks.iter().any(|b| b.hash == *hash) {
			return Err(StateDbError::BlockAlreadyExists)
		}

		let index = level.available_index();
		if index >= OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN && index == level.span() {
			let key = to_meta_key(OVERLAY_LEVEL_SPAN, &number);
			let new_span = index + 1;
			commit.meta.inserted.push((key, new_span.encode()));
		}

		let journal_key = to_journal_key(number, index);
		let inserted = changeset.inserted.iter().map(|(k, _)| k.clone()).collect();
		let overlay = BlockOverlay {
			hash: hash.clone(),
			journal_index: index,
			journal_key: journal_key.clone(),
			inserted,
			deleted: changeset.deleted.clone(),
		};
		level.push(overlay);
		self.parents.insert(hash.clone(), parent_hash.clone());
		let journal_record = JournalRecord {
			hash: hash.clone(),
			parent_hash: parent_hash.clone(),
			inserted: changeset.inserted,
			deleted: changeset.deleted,
		};

		commit.meta.inserted.push((journal_key, journal_record.encode()));
		trace!(
			target: LOG_TARGET,
			"Inserted uncanonicalized changeset {}.{} {:?} ({} inserted, {} deleted)",
			number,
			index,
			hash,
			journal_record.inserted.len(),
			journal_record.deleted.len()
		);
		insert_values(&mut self.values, journal_record.inserted);
		Ok(commit)
	}

	fn discard_journals(
		&self,
		level_index: usize,
		discarded_journals: &mut Vec<Vec<u8>>,
		hash: &BlockHash,
	) {
		if let Some(level) = self.levels.get(level_index) {
			level.blocks.iter().for_each(|overlay| {
				let parent = self
					.parents
					.get(&overlay.hash)
					.expect("there is a parent entry for each entry in levels; qed")
					.clone();
				if parent == *hash {
					discarded_journals.push(overlay.journal_key.clone());
					self.discard_journals(level_index + 1, discarded_journals, &overlay.hash);
				}
			});
		}
	}

	fn front_block_number(&self) -> u64 {
		self.last_canonicalized.as_ref().map(|&(_, n)| n + 1).unwrap_or(0)
	}

	pub fn last_canonicalized_block_number(&self) -> Option<u64> {
		self.last_canonicalized.as_ref().map(|&(_, n)| n)
	}

	/// Confirm that all changes made to commit sets are on disk. Allows for temporarily pinned
	/// blocks to be released.
	pub fn sync(&mut self) {
		let mut pinned = std::mem::take(&mut self.pinned_canonincalized);
		for hash in pinned.iter() {
			self.unpin(hash)
		}
		pinned.clear();
		// Reuse the same memory buffer
		self.pinned_canonincalized = pinned;
	}

	/// Select a top-level root and canonicalized it. Discards all sibling subtrees and the root.
	/// Add a set of changes of the canonicalized block to `CommitSet`
	/// Return the block number of the canonicalized block
	pub fn canonicalize(
		&mut self,
		hash: &BlockHash,
		commit: &mut CommitSet<Key>,
	) -> Result<u64, StateDbError> {
		trace!(target: LOG_TARGET, "Canonicalizing {:?}", hash);
		let level = match self.levels.pop_front() {
			Some(level) => level,
			None => return Err(StateDbError::InvalidBlock),
		};
		let index = level
			.blocks
			.iter()
			.position(|overlay| overlay.hash == *hash)
			.ok_or(StateDbError::InvalidBlock)?;

		// No failures are possible beyond this point.

		// Force pin canonicalized block so that it is no discarded immediately
		self.pin(hash);
		self.pinned_canonincalized.push(hash.clone());

		let mut discarded_journals = Vec::new();
		let span = level.span();
		for (i, overlay) in level.blocks.into_iter().enumerate() {
			let mut pinned_children = 0;
			// That's the one we need to canonicalize
			if i == index {
				commit.data.inserted.extend(overlay.inserted.iter().map(|k| {
					(
						k.clone(),
						self.values
							.get(k)
							.expect("For each key in overlays there's a value in values")
							.1
							.clone(),
					)
				}));
				commit.data.deleted.extend(overlay.deleted.clone());
			} else {
				// Discard this overlay
				self.discard_journals(0, &mut discarded_journals, &overlay.hash);
				pinned_children = discard_descendants(
					&mut self.levels.as_mut_slices(),
					&mut self.values,
					&mut self.parents,
					&self.pinned,
					&mut self.pinned_insertions,
					&overlay.hash,
				);
			}
			if self.pinned.contains_key(&overlay.hash) {
				pinned_children += 1;
			}
			if pinned_children != 0 {
				self.pinned_insertions
					.insert(overlay.hash.clone(), (overlay.inserted, pinned_children));
			} else {
				self.parents.remove(&overlay.hash);
				discard_values(&mut self.values, overlay.inserted);
			}
			discarded_journals.push(overlay.journal_key.clone());
		}
		commit.meta.deleted.append(&mut discarded_journals);
		if span > OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN {
			let key = to_meta_key(OVERLAY_LEVEL_SPAN, &self.front_block_number());
			commit.meta.deleted.push(key);
		}

		let canonicalized = (hash.clone(), self.front_block_number());
		commit
			.meta
			.inserted
			.push((to_meta_key(LAST_CANONICAL, &()), canonicalized.encode()));
		trace!(target: LOG_TARGET, "Discarding {} records", commit.meta.deleted.len());

		let num = canonicalized.1;
		self.last_canonicalized = Some(canonicalized);
		Ok(num)
	}

	/// Get a value from the node overlay. This searches in every existing changeset.
	pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<DBValue>
	where
		Key: std::borrow::Borrow<Q>,
		Q: std::hash::Hash + Eq,
	{
		self.values.get(key).map(|v| v.1.clone())
	}

	/// Check if the block is in the canonicalization queue.
	pub fn have_block(&self, hash: &BlockHash) -> bool {
		self.parents.contains_key(hash)
	}

	/// Revert a single level. Returns commit set that deletes the journal or `None` if not
	/// possible.
	pub fn revert_one(&mut self) -> Option<CommitSet<Key>> {
		let last_block_number = self.front_block_number() + self.levels.len() as u64 - 1;
		self.levels.pop_back().map(|level| {
			let mut commit = CommitSet::default();
			for overlay in level.blocks.into_iter() {
				commit.meta.deleted.push(overlay.journal_key);
				self.parents.remove(&overlay.hash);
				discard_values(&mut self.values, overlay.inserted);
			}
			if level.span > OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN {
				let key = to_meta_key(OVERLAY_LEVEL_SPAN, &last_block_number);
				commit.meta.deleted.push(key);
			}
			commit
		})
	}

	/// Revert a single block. Returns commit set that deletes the journal or `None` if not
	/// possible.
	pub fn remove(&mut self, hash: &BlockHash) -> Option<CommitSet<Key>> {
		let mut commit = CommitSet::default();
		let level_count = self.levels.len();
		for (level_index, level) in self.levels.iter_mut().enumerate().rev() {
			let index = match level.blocks.iter().position(|overlay| &overlay.hash == hash) {
				Some(index) => index,
				None => continue,
			};
			// Check that it does not have any children
			if (level_index != level_count - 1) && self.parents.values().any(|h| h == hash) {
				log::debug!(target: LOG_TARGET, "Trying to remove block {:?} with children", hash);
				return None
			}
			let overlay = level.remove(index);
			commit.meta.deleted.push(overlay.journal_key);
			self.parents.remove(&overlay.hash);
			discard_values(&mut self.values, overlay.inserted);
			break
		}
		if self.levels.back().map_or(false, |l| l.blocks.is_empty()) {
			let last_block_number = self.front_block_number() + level_count as u64 - 1;
			if self.levels.back().map_or(0, |l| l.span) > OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN {
				let key = to_meta_key(OVERLAY_LEVEL_SPAN, &last_block_number);
				commit.meta.deleted.push(key);
			}
			self.levels.pop_back();
		}
		if !commit.meta.deleted.is_empty() {
			Some(commit)
		} else {
			None
		}
	}

	/// Pin state values in memory
	pub fn pin(&mut self, hash: &BlockHash) {
		let refs = self.pinned.entry(hash.clone()).or_default();
		if *refs == 0 {
			trace!(target: LOG_TARGET_PIN, "Pinned non-canon block: {:?}", hash);
		}
		*refs += 1;
	}

	/// Discard pinned state
	pub fn unpin(&mut self, hash: &BlockHash) {
		let removed = match self.pinned.entry(hash.clone()) {
			Entry::Occupied(mut entry) => {
				*entry.get_mut() -= 1;
				if *entry.get() == 0 {
					entry.remove();
					true
				} else {
					false
				}
			},
			Entry::Vacant(_) => false,
		};

		if removed {
			let mut parent = Some(hash.clone());
			while let Some(hash) = parent {
				parent = self.parents.get(&hash).cloned();
				match self.pinned_insertions.entry(hash.clone()) {
					Entry::Occupied(mut entry) => {
						entry.get_mut().1 -= 1;
						if entry.get().1 == 0 {
							let (inserted, _) = entry.remove();
							trace!(
								target: LOG_TARGET_PIN,
								"Discarding unpinned non-canon block: {:?}",
								hash
							);
							discard_values(&mut self.values, inserted);
							self.parents.remove(&hash);
						}
					},
					Entry::Vacant(_) => break,
				};
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{to_journal_key, NonCanonicalOverlay};
	use crate::{
		noncanonical::{LAST_CANONICAL, OVERLAY_LEVEL_SPAN, OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN},
		test::{make_changeset, make_db},
		to_meta_key, ChangeSet, CommitSet, MetaDb, StateDbError,
	};
	use codec::Encode;
	use sp_core::H256;
	use std::collections::HashSet;

	fn contains(overlay: &NonCanonicalOverlay<H256, H256>, key: u64) -> bool {
		overlay.get(&H256::from_low_u64_be(key)) ==
			Some(H256::from_low_u64_be(key).as_bytes().to_vec())
	}

	#[test]
	fn created_from_empty_db() {
		let db = make_db(&[]);
		let overlay: NonCanonicalOverlay<H256, H256> = NonCanonicalOverlay::new(&db).unwrap();
		assert_eq!(overlay.last_canonicalized, None);
		assert!(overlay.levels.is_empty());
		assert!(overlay.parents.is_empty());
	}

	#[test]
	#[should_panic]
	fn canonicalize_empty_panics() {
		let db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		let mut commit = CommitSet::default();
		overlay.canonicalize(&H256::default(), &mut commit).unwrap();
	}

	#[test]
	#[should_panic]
	fn insert_ahead_panics() {
		let db = make_db(&[]);
		let h1 = H256::random();
		let h2 = H256::random();
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		overlay.insert(&h1, 2, &H256::default(), ChangeSet::default()).unwrap();
		overlay.insert(&h2, 1, &h1, ChangeSet::default()).unwrap();
	}

	#[test]
	#[should_panic]
	fn insert_behind_panics() {
		let h1 = H256::random();
		let h2 = H256::random();
		let db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		overlay.insert(&h1, 1, &H256::default(), ChangeSet::default()).unwrap();
		overlay.insert(&h2, 3, &h1, ChangeSet::default()).unwrap();
	}

	#[test]
	#[should_panic]
	fn insert_unknown_parent_panics() {
		let db = make_db(&[]);
		let h1 = H256::random();
		let h2 = H256::random();
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		overlay.insert(&h1, 1, &H256::default(), ChangeSet::default()).unwrap();
		overlay.insert(&h2, 2, &H256::default(), ChangeSet::default()).unwrap();
	}

	#[test]
	fn insert_existing_fails() {
		let db = make_db(&[]);
		let h1 = H256::random();
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		overlay.insert(&h1, 2, &H256::default(), ChangeSet::default()).unwrap();
		assert!(matches!(
			overlay.insert(&h1, 2, &H256::default(), ChangeSet::default()),
			Err(StateDbError::BlockAlreadyExists)
		));
	}

	#[test]
	#[should_panic]
	fn canonicalize_unknown_panics() {
		let h1 = H256::random();
		let h2 = H256::random();
		let db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		overlay.insert(&h1, 1, &H256::default(), ChangeSet::default()).unwrap();
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h2, &mut commit).unwrap();
	}

	#[test]
	fn insert_canonicalize_one() {
		let h1 = H256::random();
		let mut db = make_db(&[1, 2]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		let changeset = make_changeset(&[3, 4], &[2]);
		let insertion = overlay.insert(&h1, 1, &H256::default(), changeset.clone()).unwrap();
		assert_eq!(insertion.data.inserted.len(), 0);
		assert_eq!(insertion.data.deleted.len(), 0);
		assert_eq!(insertion.meta.inserted.len(), 2);
		assert_eq!(insertion.meta.deleted.len(), 0);
		db.commit(&insertion);
		let mut finalization = CommitSet::default();
		overlay.canonicalize(&h1, &mut finalization).unwrap();
		assert_eq!(finalization.data.inserted.len(), changeset.inserted.len());
		assert_eq!(finalization.data.deleted.len(), changeset.deleted.len());
		assert_eq!(finalization.meta.inserted.len(), 1);
		assert_eq!(finalization.meta.deleted.len(), 1);
		db.commit(&finalization);
		assert!(db.data_eq(&make_db(&[1, 3, 4])));
	}

	#[test]
	fn restore_from_journal() {
		let h1 = H256::random();
		let h2 = H256::random();
		let mut db = make_db(&[1, 2]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(
			&overlay
				.insert(&h1, 10, &H256::default(), make_changeset(&[3, 4], &[2]))
				.unwrap(),
		);
		db.commit(&overlay.insert(&h2, 11, &h1, make_changeset(&[5], &[3])).unwrap());
		assert_eq!(db.meta_len(), 3); //TODO

		let overlay2 = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert_eq!(overlay.levels, overlay2.levels);
		assert_eq!(overlay.parents, overlay2.parents);
		assert_eq!(overlay.last_canonicalized, overlay2.last_canonicalized);
	}

	#[test]
	fn restore_from_journal_after_canonicalize() {
		let h1 = H256::random();
		let h2 = H256::random();
		let mut db = make_db(&[1, 2]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(
			&overlay
				.insert(&h1, 10, &H256::default(), make_changeset(&[3, 4], &[2]))
				.unwrap(),
		);
		db.commit(&overlay.insert(&h2, 11, &h1, make_changeset(&[5], &[3])).unwrap());
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h1, &mut commit).unwrap();
		overlay.unpin(&h1);
		db.commit(&commit);
		assert_eq!(overlay.levels.len(), 1);

		let overlay2 = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert_eq!(overlay.levels, overlay2.levels);
		assert_eq!(overlay.parents, overlay2.parents);
		assert_eq!(overlay.last_canonicalized, overlay2.last_canonicalized);
	}

	#[test]
	fn insert_canonicalize_two() {
		let h1 = H256::random();
		let h2 = H256::random();
		let mut db = make_db(&[1, 2, 3, 4]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		let changeset1 = make_changeset(&[5, 6], &[2]);
		let changeset2 = make_changeset(&[7, 8], &[5, 3]);
		db.commit(&overlay.insert(&h1, 1, &H256::default(), changeset1).unwrap());
		assert!(contains(&overlay, 5));
		db.commit(&overlay.insert(&h2, 2, &h1, changeset2).unwrap());
		assert!(contains(&overlay, 7));
		assert!(contains(&overlay, 5));
		assert_eq!(overlay.levels.len(), 2);
		assert_eq!(overlay.parents.len(), 2);
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h1, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert!(!contains(&overlay, 5));
		assert!(contains(&overlay, 7));
		assert_eq!(overlay.levels.len(), 1);
		assert_eq!(overlay.parents.len(), 1);
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h2, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert_eq!(overlay.levels.len(), 0);
		assert_eq!(overlay.parents.len(), 0);
		assert!(db.data_eq(&make_db(&[1, 4, 6, 7, 8])));
	}

	#[test]
	fn insert_same_key() {
		let mut db = make_db(&[]);
		let (h_1, c_1) = (H256::random(), make_changeset(&[1], &[]));
		let (h_2, c_2) = (H256::random(), make_changeset(&[1], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_1, 1, &H256::default(), c_1).unwrap());
		db.commit(&overlay.insert(&h_2, 1, &H256::default(), c_2).unwrap());
		assert!(contains(&overlay, 1));
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_1, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert!(!contains(&overlay, 1));
	}

	#[test]
	fn insert_and_canonicalize() {
		let h1 = H256::random();
		let h2 = H256::random();
		let h3 = H256::random();
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		let changeset = make_changeset(&[], &[]);
		db.commit(&overlay.insert(&h1, 1, &H256::default(), changeset.clone()).unwrap());
		db.commit(&overlay.insert(&h2, 2, &h1, changeset.clone()).unwrap());
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h1, &mut commit).unwrap();
		overlay.canonicalize(&h2, &mut commit).unwrap();
		db.commit(&commit);
		db.commit(&overlay.insert(&h3, 3, &h2, changeset.clone()).unwrap());
		assert_eq!(overlay.levels.len(), 1);
	}

	#[test]
	fn complex_tree() {
		let mut db = make_db(&[]);

		#[rustfmt::skip]
		// - 1 - 1_1 - 1_1_1
		//     \ 1_2 - 1_2_1
		//           \ 1_2_2
		//           \ 1_2_3
		//
		// - 2 - 2_1 - 2_1_1
		//     \ 2_2
		//
		// 1_2_2 is the winner

		let (h_1, c_1) = (H256::random(), make_changeset(&[1], &[]));
		let (h_2, c_2) = (H256::random(), make_changeset(&[2], &[]));

		let (h_1_1, c_1_1) = (H256::random(), make_changeset(&[11], &[]));
		let (h_1_2, c_1_2) = (H256::random(), make_changeset(&[12], &[]));
		let (h_2_1, c_2_1) = (H256::random(), make_changeset(&[21], &[]));
		let (h_2_2, c_2_2) = (H256::random(), make_changeset(&[22], &[]));

		let (h_1_1_1, c_1_1_1) = (H256::random(), make_changeset(&[111], &[]));
		let (h_1_2_1, c_1_2_1) = (H256::random(), make_changeset(&[121], &[]));
		let (h_1_2_2, c_1_2_2) = (H256::random(), make_changeset(&[122], &[]));
		let (h_1_2_3, c_1_2_3) = (H256::random(), make_changeset(&[123], &[]));
		let (h_2_1_1, c_2_1_1) = (H256::random(), make_changeset(&[211], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_1, 1, &H256::default(), c_1).unwrap());

		db.commit(&overlay.insert(&h_1_1, 2, &h_1, c_1_1).unwrap());
		db.commit(&overlay.insert(&h_1_2, 2, &h_1, c_1_2).unwrap());

		db.commit(&overlay.insert(&h_2, 1, &H256::default(), c_2).unwrap());

		db.commit(&overlay.insert(&h_2_1, 2, &h_2, c_2_1).unwrap());
		db.commit(&overlay.insert(&h_2_2, 2, &h_2, c_2_2).unwrap());

		db.commit(&overlay.insert(&h_1_1_1, 3, &h_1_1, c_1_1_1).unwrap());
		db.commit(&overlay.insert(&h_1_2_1, 3, &h_1_2, c_1_2_1).unwrap());
		db.commit(&overlay.insert(&h_1_2_2, 3, &h_1_2, c_1_2_2).unwrap());
		db.commit(&overlay.insert(&h_1_2_3, 3, &h_1_2, c_1_2_3).unwrap());
		db.commit(&overlay.insert(&h_2_1_1, 3, &h_2_1, c_2_1_1).unwrap());

		assert!(contains(&overlay, 2));
		assert!(contains(&overlay, 11));
		assert!(contains(&overlay, 21));
		assert!(contains(&overlay, 111));
		assert!(contains(&overlay, 122));
		assert!(contains(&overlay, 211));
		assert_eq!(overlay.levels.len(), 3);
		assert_eq!(overlay.parents.len(), 11);
		assert_eq!(overlay.last_canonicalized, Some((H256::default(), 0)));

		// check if restoration from journal results in the same tree
		let overlay2 = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert_eq!(overlay.levels, overlay2.levels);
		assert_eq!(overlay.parents, overlay2.parents);
		assert_eq!(overlay.last_canonicalized, overlay2.last_canonicalized);

		// canonicalize 1. 2 and all its children should be discarded
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_1, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert_eq!(overlay.levels.len(), 2);
		assert_eq!(overlay.parents.len(), 6);
		assert!(!contains(&overlay, 1));
		assert!(!contains(&overlay, 2));
		assert!(!contains(&overlay, 21));
		assert!(!contains(&overlay, 22));
		assert!(!contains(&overlay, 211));
		assert!(contains(&overlay, 111));
		assert!(!contains(&overlay, 211));
		// check that journals are deleted
		assert!(db.get_meta(&to_journal_key(1, 0)).unwrap().is_none());
		assert!(db.get_meta(&to_journal_key(1, 1)).unwrap().is_none());
		assert!(db.get_meta(&to_journal_key(2, 1)).unwrap().is_some());
		assert!(db.get_meta(&to_journal_key(2, 2)).unwrap().is_none());
		assert!(db.get_meta(&to_journal_key(2, 3)).unwrap().is_none());

		// canonicalize 1_2. 1_1 and all its children should be discarded
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_1_2, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert_eq!(overlay.levels.len(), 1);
		assert_eq!(overlay.parents.len(), 3);
		assert!(!contains(&overlay, 11));
		assert!(!contains(&overlay, 111));
		assert!(contains(&overlay, 121));
		assert!(contains(&overlay, 122));
		assert!(contains(&overlay, 123));
		assert!(overlay.have_block(&h_1_2_1));
		assert!(!overlay.have_block(&h_1_2));
		assert!(!overlay.have_block(&h_1_1));
		assert!(!overlay.have_block(&h_1_1_1));

		// canonicalize 1_2_2
		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_1_2_2, &mut commit).unwrap();
		db.commit(&commit);
		overlay.sync();
		assert_eq!(overlay.levels.len(), 0);
		assert_eq!(overlay.parents.len(), 0);
		assert!(db.data_eq(&make_db(&[1, 12, 122])));
		assert_eq!(overlay.last_canonicalized, Some((h_1_2_2, 3)));
	}

	#[test]
	fn insert_revert() {
		let h1 = H256::random();
		let h2 = H256::random();
		let mut db = make_db(&[1, 2, 3, 4]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert!(overlay.revert_one().is_none());
		let changeset1 = make_changeset(&[5, 6], &[2]);
		let changeset2 = make_changeset(&[7, 8], &[5, 3]);
		db.commit(&overlay.insert(&h1, 1, &H256::default(), changeset1).unwrap());
		db.commit(&overlay.insert(&h2, 2, &h1, changeset2).unwrap());
		assert!(contains(&overlay, 7));
		db.commit(&overlay.revert_one().unwrap());
		assert_eq!(overlay.parents.len(), 1);
		assert!(contains(&overlay, 5));
		assert!(!contains(&overlay, 7));
		db.commit(&overlay.revert_one().unwrap());
		assert_eq!(overlay.levels.len(), 0);
		assert_eq!(overlay.parents.len(), 0);
		assert!(overlay.revert_one().is_none());
	}

	#[test]
	fn keeps_pinned() {
		let mut db = make_db(&[]);

		#[rustfmt::skip]
		// - 0 - 1_1
		//     \ 1_2

		let (h_1, c_1) = (H256::random(), make_changeset(&[1], &[]));
		let (h_2, c_2) = (H256::random(), make_changeset(&[2], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_1, 1, &H256::default(), c_1).unwrap());
		db.commit(&overlay.insert(&h_2, 1, &H256::default(), c_2).unwrap());

		overlay.pin(&h_1);

		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_2, &mut commit).unwrap();
		db.commit(&commit);
		assert!(contains(&overlay, 1));
		overlay.unpin(&h_1);
		assert!(!contains(&overlay, 1));
	}

	#[test]
	fn keeps_pinned_ref_count() {
		let mut db = make_db(&[]);

		#[rustfmt::skip]
		// - 0 - 1_1
		//     \ 1_2
		//     \ 1_3

		// 1_1 and 1_2 both make the same change
		let (h_1, c_1) = (H256::random(), make_changeset(&[1], &[]));
		let (h_2, c_2) = (H256::random(), make_changeset(&[1], &[]));
		let (h_3, c_3) = (H256::random(), make_changeset(&[], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_1, 1, &H256::default(), c_1).unwrap());
		db.commit(&overlay.insert(&h_2, 1, &H256::default(), c_2).unwrap());
		db.commit(&overlay.insert(&h_3, 1, &H256::default(), c_3).unwrap());

		overlay.pin(&h_1);

		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_3, &mut commit).unwrap();
		db.commit(&commit);

		assert!(contains(&overlay, 1));
		overlay.unpin(&h_1);
		assert!(!contains(&overlay, 1));
	}

	#[test]
	fn pins_canonicalized() {
		let mut db = make_db(&[]);

		let (h_1, c_1) = (H256::random(), make_changeset(&[1], &[]));
		let (h_2, c_2) = (H256::random(), make_changeset(&[2], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_1, 1, &H256::default(), c_1).unwrap());
		db.commit(&overlay.insert(&h_2, 2, &h_1, c_2).unwrap());

		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_1, &mut commit).unwrap();
		overlay.canonicalize(&h_2, &mut commit).unwrap();
		assert!(contains(&overlay, 1));
		assert!(contains(&overlay, 2));
		db.commit(&commit);
		overlay.sync();
		assert!(!contains(&overlay, 1));
		assert!(!contains(&overlay, 2));
	}

	#[test]
	fn pin_keeps_parent() {
		let mut db = make_db(&[]);

		#[rustfmt::skip]
		// - 0 - 1_1 - 2_1
		//     \ 1_2

		let (h_11, c_11) = (H256::random(), make_changeset(&[1], &[]));
		let (h_12, c_12) = (H256::random(), make_changeset(&[], &[]));
		let (h_21, c_21) = (H256::random(), make_changeset(&[], &[]));

		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&h_11, 1, &H256::default(), c_11).unwrap());
		db.commit(&overlay.insert(&h_12, 1, &H256::default(), c_12).unwrap());
		db.commit(&overlay.insert(&h_21, 2, &h_11, c_21).unwrap());

		overlay.pin(&h_21);

		let mut commit = CommitSet::default();
		overlay.canonicalize(&h_12, &mut commit).unwrap();
		db.commit(&commit);

		assert!(contains(&overlay, 1));
		overlay.unpin(&h_21);
		assert!(!contains(&overlay, 1));
		overlay.unpin(&h_12);
		assert!(overlay.pinned.is_empty());
	}

	#[test]
	fn restore_from_journal_after_canonicalize_no_first() {
		// This test discards a branch that is journaled under a non-zero index on level 1,
		// making sure all journals are loaded for each level even if some of them are missing.
		let root = H256::random();
		let h1 = H256::random();
		let h2 = H256::random();
		let h11 = H256::random();
		let h21 = H256::random();
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&root, 10, &H256::default(), make_changeset(&[], &[])).unwrap());
		db.commit(&overlay.insert(&h1, 11, &root, make_changeset(&[1], &[])).unwrap());
		db.commit(&overlay.insert(&h2, 11, &root, make_changeset(&[2], &[])).unwrap());
		db.commit(&overlay.insert(&h11, 12, &h1, make_changeset(&[11], &[])).unwrap());
		db.commit(&overlay.insert(&h21, 12, &h2, make_changeset(&[21], &[])).unwrap());
		let mut commit = CommitSet::default();
		overlay.canonicalize(&root, &mut commit).unwrap();
		overlay.canonicalize(&h2, &mut commit).unwrap(); // h11 should stay in the DB
		db.commit(&commit);
		assert_eq!(overlay.levels.len(), 1);
		assert!(contains(&overlay, 21));
		assert!(!contains(&overlay, 11));
		assert!(db.get_meta(&to_journal_key(12, 1)).unwrap().is_some());

		// Restore into a new overlay and check that journaled value exists.
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert!(contains(&overlay, 21));

		let mut commit = CommitSet::default();
		overlay.canonicalize(&h21, &mut commit).unwrap(); // h11 should stay in the DB
		db.commit(&commit);
		overlay.sync();
		assert!(!contains(&overlay, 21));
	}

	#[test]
	fn index_reuse() {
		// This test discards a branch that is journaled under a non-zero index on level 1,
		// making sure all journals are loaded for each level even if some of them are missing.
		let root = H256::random();
		let h1 = H256::random();
		let h2 = H256::random();
		let h11 = H256::random();
		let h21 = H256::random();
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&root, 10, &H256::default(), make_changeset(&[], &[])).unwrap());
		db.commit(&overlay.insert(&h1, 11, &root, make_changeset(&[1], &[])).unwrap());
		db.commit(&overlay.insert(&h2, 11, &root, make_changeset(&[2], &[])).unwrap());
		db.commit(&overlay.insert(&h11, 12, &h1, make_changeset(&[11], &[])).unwrap());
		db.commit(&overlay.insert(&h21, 12, &h2, make_changeset(&[21], &[])).unwrap());
		let mut commit = CommitSet::default();
		overlay.canonicalize(&root, &mut commit).unwrap();
		overlay.canonicalize(&h2, &mut commit).unwrap(); // h11 should stay in the DB
		db.commit(&commit);

		// add another block at top level. It should reuse journal index 0 of previously discarded
		// block
		let h22 = H256::random();
		db.commit(&overlay.insert(&h22, 12, &h2, make_changeset(&[22], &[])).unwrap());
		assert_eq!(overlay.levels[0].blocks[0].journal_index, 1);
		assert_eq!(overlay.levels[0].blocks[1].journal_index, 0);

		// Restore into a new overlay and check that journaled value exists.
		let overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		assert_eq!(overlay.parents.len(), 2);
		assert!(contains(&overlay, 21));
		assert!(contains(&overlay, 22));
	}

	#[test]
	fn remove_works() {
		let root = H256::random();
		let h1 = H256::random();
		let h2 = H256::random();
		let h11 = H256::random();
		let h21 = H256::random();
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<H256, H256>::new(&db).unwrap();
		db.commit(&overlay.insert(&root, 10, &H256::default(), make_changeset(&[], &[])).unwrap());
		db.commit(&overlay.insert(&h1, 11, &root, make_changeset(&[1], &[])).unwrap());
		db.commit(&overlay.insert(&h2, 11, &root, make_changeset(&[2], &[])).unwrap());
		db.commit(&overlay.insert(&h11, 12, &h1, make_changeset(&[11], &[])).unwrap());
		db.commit(&overlay.insert(&h21, 12, &h2, make_changeset(&[21], &[])).unwrap());
		assert!(overlay.remove(&h1).is_none());
		assert!(overlay.remove(&h2).is_none());
		assert_eq!(overlay.levels.len(), 3);

		db.commit(&overlay.remove(&h11).unwrap());
		assert!(!contains(&overlay, 11));

		db.commit(&overlay.remove(&h21).unwrap());
		assert_eq!(overlay.levels.len(), 2);

		db.commit(&overlay.remove(&h2).unwrap());
		assert!(!contains(&overlay, 2));
	}

	#[test]
	fn insert_canonicalize_long_span() {
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<(u64, u64), H256>::new(&db).unwrap();
		let indices_per_level = OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN+2;

		for i in 0..indices_per_level {
			let changeset = make_changeset(&[i], &[]);
			let insertion = overlay.insert(&(1,i), 1, &(0,0), changeset).unwrap();
			db.commit(&insertion);
			let expected_insertion_meta_len = match i {
				0 => 2,                                       // last canonical, journal key
				OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN.. => 2, // journal key, span
				_ => 1,                                       // just a journal key
			};
			assert_eq!(insertion.meta.inserted.len(), expected_insertion_meta_len);
		}

		let mut commit = CommitSet::default();
		overlay.canonicalize(&(1,indices_per_level-1), &mut commit).unwrap();

		let expected_meta_deleted = (0..indices_per_level)
			.map(|i| to_journal_key(1, i))
			.chain([to_meta_key(OVERLAY_LEVEL_SPAN, &1u64)])
			.collect::<HashSet<_>>();
		assert_eq!(commit.meta.inserted.len(), 1);
		assert_eq!(HashSet::from_iter(commit.meta.deleted), expected_meta_deleted);
	}

	#[test]
	fn restore_from_journal_long_span() {
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<(u64, u64), H256>::new(&db).unwrap();
		let indices_per_level = OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN+2;

		for i in 0..indices_per_level {
			let changeset = make_changeset(&[i], &[]);
			let insertion = overlay.insert(&(1,i), 1, &(0,0), changeset).unwrap();
			db.commit(&insertion);
		}

		for i in 0..indices_per_level-1 {
			db.commit(&overlay.remove(&(1,i)).unwrap());
		}

		// Restore into a new overlay and check that journaled value exists.
		let mut overlay = NonCanonicalOverlay::<(u64, u64), H256>::new(&db).unwrap();
		let mut commit = CommitSet::default();
		overlay.canonicalize(&(1,indices_per_level-1), &mut commit).unwrap();
		db.commit(&commit);

		assert!(db.data_eq(& make_db(&[indices_per_level-1])));
		assert_eq!(commit.meta.inserted.len(), 1);
		assert_eq!(commit.meta.deleted.len(), 2); // journal key, span
	}

	#[test]
	fn overlay_level_span_entries_are_cleared_from_db() {
		let mut db = make_db(&[]);
		let mut overlay = NonCanonicalOverlay::<(u64, u64), H256>::new(&db).unwrap();
		let indices_per_level = OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN + 1;
		for level in 1..4 {
			for index in 0..indices_per_level {
				let changeset = make_changeset(&[1], &[]);
				let insertion =
					overlay.insert(&(level, index), level, &(level - 1, 0), changeset).unwrap();
				db.commit(&insertion);
			}
		}

		db.commit(&overlay.revert_one().unwrap());
		for index in 0..indices_per_level {
			db.commit(&overlay.remove(&(2, index)).unwrap());
		}
		for index in OVERLAY_LEVEL_STORE_SPANS_LONGER_THAN..indices_per_level {
			db.commit(&overlay.remove(&(1, index)).unwrap());
		}
		let mut overlay = NonCanonicalOverlay::<(u64, u64), H256>::new(&db).unwrap();
		let mut commit = CommitSet::default();
		overlay.canonicalize(&(1, 1), &mut commit).unwrap();
		db.commit(&commit);

		assert_eq!(db.meta_len(), 1);
		assert_eq!(
			db.get_meta(&to_meta_key(LAST_CANONICAL, &())),
			Ok(Some((&(1u64, 1u64), 1u64).encode()))
		);
	}
}
