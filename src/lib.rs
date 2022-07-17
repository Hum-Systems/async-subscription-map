use async_observable::Observable;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::iter::IntoIterator;
use std::ops::{Deref, DerefMut};
use async_std::sync::{Mutex, MutexGuard};
use std::sync::Arc;

/// A concurrent and self cleaning map of observable values
#[derive(Clone, Debug)]
pub struct SubscriptionMap<K, V>(Arc<Mutex<BTreeMap<K, SubscriptionEntry<V>>>>)
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug;

/// A single observable entry and its subscription count
#[derive(Clone, Debug)]
struct SubscriptionEntry<V>
where
    V: Clone + Debug,
{
    observable: Observable<V>,
    rc: usize,
}

impl<V> SubscriptionEntry<V>
where
    V: Clone + Debug,
{
    pub fn new(value: V) -> Self {
        Self {
            observable: Observable::new(value),
            rc: 0,
        }
    }
}

impl<K, V> SubscriptionMap<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(BTreeMap::new())))
    }

    pub fn get_or_insert(&self, key: K, value: V) -> SubscriptionRef<K, V> {
        let mut map = self.lock_inner();
        let entry = {
            let entry = SubscriptionEntry::new(value);
            map.entry(key.clone()).or_insert(entry)
        };

        SubscriptionRef::new(key, self.clone(), entry).unwrap()
    }

    pub fn keys(&self) -> Keys<K, V> {
        Keys::from(self)
    }

    #[cfg(test)]
    fn snapshot(&self) -> BTreeMap<K, SubscriptionEntry<V>> {
        self.lock_inner().deref().clone()
    }

    fn remove(&self, key: &K) -> anyhow::Result<()> {
        let mut map = self.lock_inner();

        let entry = map
            .get(key)
            .with_context(|| format!("unable remove not present key {:?} in {:#?}", key, self))?;

        assert!(
            entry.rc == 0,
            "invalid removal of referenced subscription at {:?}",
            key
        );

        map.remove(key);

        Ok(())
    }

    fn lock_inner(&self) -> MutexGuard<'_, BTreeMap<K, SubscriptionEntry<V>>> {
        match self.0.lock() {
            Ok(guard) => guard,
            Err(e) => e.into_inner(),
        }
    }
}

impl<K, V> SubscriptionMap<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug + Eq,
{
    /// Check if the provided value differs from the observable and return the info if a publish
    /// was made.
    pub fn publish_if_changed(&self, key: &K, value: V) -> anyhow::Result<bool> {
        let mut map = self.lock_inner();
        let entry = map
            .get_mut(key)
            .with_context(|| format!("unable publish new version of not present key {:?}", key))?;

        Ok(entry.observable.publish_if_changed(value))
    }

    pub fn modify_and_publish<F, R>(&self, key: &K, modify: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut V) -> R,
    {
        let mut map = self.lock_inner();
        let entry = map
            .get_mut(key)
            .with_context(|| format!("unable modify not present key {:?}", key))?;

        entry.observable.modify(|v| {
            modify(v);
        });

        Ok(())
    }
}

impl<K, V> Default for SubscriptionMap<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> IntoIterator for &SubscriptionMap<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    type Item = K;
    type IntoIter = Keys<K, V>;

    fn into_iter(self) -> Self::IntoIter {
        Keys::from(self)
    }
}

/// An on-demand locking iterator over keys of a subscription map
///
/// ## Warning
/// This is not comparable to a snapshot of all keys! It will be affected by
/// concurrent access to the underlying map due to the fact that it doesnt copy
/// anything, it only iterates through the parent map using a cursor.
#[derive(Debug)]
pub struct Keys<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    map: SubscriptionMap<K, V>,
    previous: Option<K>,
    done: bool,
}

impl<K, V> Frm<&SubscriptionMap<K, V>> for Keys<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    fn from(map: &SubscriptionMap<K, V>) -> Self {
        Self {
            map: map.clone(),
            previous: None,
            done: false,
        }
    }
}

impl<K, V> Iterator for Keys<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        use std::ops::Bound::{Excluded, Unbounded};

        if self.done {
            return None;
        }

        let bounds = match self.previous.clone() {
            None => (Unbounded, Unbounded),
            Some(key) => (Excluded(key), Unbounded),
        };

        let key = self
            .map
            .lock_inner()
            .range(bounds)
            .next()
            .map(|(k, _)| k.clone());

        self.previous = key.clone();
        self.done = key.is_none();

        key
    }
}

/// A transparent wrapper for the underlying subscription in the map
/// which manages the subscription count and removes the observable if no one
/// holds a subscription to it.
#[derive(Debug)]
#[must_use = "entries are removed as soon as no one subscribes to them"]
pub struct SubscriptionRef<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    key: K,
    owner: SubscriptionMap<K, V>,
    observable: Observable<V>,
}

impl<K, V> SubscriptionRef<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    fn new(
        key: K,
        owner: SubscriptionMap<K, V>,
        entry: &mut SubscriptionEntry<V>,
    ) -> anyhow::Result<Self> {
        entry.rc += 1;

        Ok(Self {
            key,
            owner,
            observable: entry.observable.fork(),
        })
    }
}

impl<K, V> Deref for SubscriptionRef<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    type Target = Observable<V>;

    fn deref(&self) -> &Self::Target {
        &self.observable
    }
}

impl<K, V> DerefMut for SubscriptionRef<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.observable
    }
}

impl<K, V> Drop for SubscriptionRef<K, V>
where
    K: Clone + Debug + Eq + Hash + Ord,
    V: Clone + Debug,
{
    fn drop(&mut self) {
        log::info!("rc drop");

        let mut map = self.owner.lock_inner();
        let mut entry = match map.get_mut(&self.key) {
            Some(entry) => entry,
            None => {
                log::error!("could not obtain rc in subscription map {:#?}", map.deref());
                return;
            }
        };

        entry.rc -= 1;

        if entry.rc == 0 {
            drop(map);
            let res = self.owner.remove(&self.key);

            if let Err(e) = res {
                log::error!("error occurred while cleanup subscription ref {}", e);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::SubscriptionMap;

    macro_rules! assert_map_len {
        ($map:ident, $len:expr) => {
            assert_eq!($map.snapshot().len(), $len);
        };
    }

    macro_rules! assert_ref_count {
        ($map:ident, $key:expr, $rc:expr) => {
            assert_eq!($map.snapshot().get($key).unwrap().rc, $rc);
        };
    }

    #[test]
    fn should_immediately_remove_unused() {
        let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
        assert_map_len!(map, 0);

        let _ = map.get_or_insert(1, 1);
        assert_map_len!(map, 0);

        let _ = map.get_or_insert(2, 2);
        assert_map_len!(map, 0);
    }

    #[test]
    fn should_remove_entries_on_ref_drop() {
        let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
        assert_map_len!(map, 0);

        let ref_one = map.get_or_insert(1, 1);
        assert_map_len!(map, 1);

        let ref_two = map.get_or_insert(2, 2);
        assert_map_len!(map, 2);

        drop(ref_one);
        assert_map_len!(map, 1);
        assert!(map.snapshot().get(&1).is_none());
        assert!(map.snapshot().get(&2).is_some());

        drop(ref_two);
        assert_map_len!(map, 0);
        assert!(map.snapshot().get(&1).is_none());
        assert!(map.snapshot().get(&2).is_none());
    }

    #[test]
    fn should_keep_track_of_ref_count() {
        let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
        assert_map_len!(map, 0);

        let ref_one = map.get_or_insert(1, 1);
        assert_ref_count!(map, &1, 1);

        let ref_two = map.get_or_insert(1, 1);
        assert_ref_count!(map, &1, 2);

        drop(ref_one);
        assert_ref_count!(map, &1, 1);

        drop(ref_two);
        assert_map_len!(map, 0);
    }

    #[test]
    #[should_panic]
    fn shouldnt_remove_if_rc_is_not_zero() {
        let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
        assert_map_len!(map, 0);

        let _ref = map.get_or_insert(1, 1);
        assert_ref_count!(map, &1, 1);

        map.remove(&1).unwrap();
    }

    mod keys {
        use super::*;

        #[test]
        fn should_be_initially_empty() {
            let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
            let mut keys = map.into_iter();
            assert_eq!(keys.next(), None);
        }

        #[test]
        fn should_be_ordered() {
            let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();

            let _0 = map.get_or_insert(0, 0);
            let _1 = map.get_or_insert(1, 1);
            let _2 = map.get_or_insert(2, 2);

            assert_map_len!(map, 3);

            let mut keys = map.into_iter();

            assert_eq!(keys.next(), Some(0));
            assert_eq!(keys.next(), Some(1));
            assert_eq!(keys.next(), Some(2));
            assert_eq!(keys.next(), None);
        }

        #[test]
        fn should_not_retrieve_new_keys_after_first_none() {
            let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();

            let _0 = map.get_or_insert(0, 0);
            assert_map_len!(map, 1);

            let mut keys = map.into_iter();
            assert_eq!(keys.next(), Some(0));
            assert_eq!(keys.next(), None);

            let _1 = map.get_or_insert(1, 1);
            assert_eq!(keys.next(), None);

            let _2 = map.get_or_insert(2, 2);
            assert_eq!(keys.next(), None);
        }

        #[test]
        fn should_retrieve_new_keys_after_first_usage() {
            let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();

            let _0 = map.get_or_insert(0, 0);
            assert_map_len!(map, 1);

            let mut keys = map.into_iter();
            assert_eq!(keys.next(), Some(0));

            let _1 = map.get_or_insert(1, 1);
            assert_eq!(keys.next(), Some(1));

            let _2 = map.get_or_insert(2, 2);
            assert_eq!(keys.next(), Some(2));
            assert_eq!(keys.next(), None);
        }

        #[test]
        fn should_retrieve_new_keys_before_first_usage() {
            let map: SubscriptionMap<usize, usize> = SubscriptionMap::new();
            let mut keys = map.into_iter();

            assert_map_len!(map, 0);

            let _0 = map.get_or_insert(0, 0);
            assert_eq!(keys.next(), Some(0));

            let _1 = map.get_or_insert(1, 1);
            assert_eq!(keys.next(), Some(1));
            assert_eq!(keys.next(), None);
        }
    }
}o
