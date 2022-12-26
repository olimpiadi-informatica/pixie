extern crate alloc;

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bijection<T, U>(Vec<(T, U)>);

impl<T, U> Bijection<T, U>
where
    T: PartialEq,
    U: PartialEq,
{
    pub fn new() -> Self {
        Bijection(Vec::new())
    }

    pub fn get_by_first(&self, t: &T) -> Option<&U> {
        self.0.iter().find(|(t1, _)| t1 == t).map(|(_, u)| u)
    }

    pub fn get_by_second(&self, u: &U) -> Option<&T> {
        self.0.iter().find(|(_, u1)| u1 == u).map(|(t, _)| t)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(T, U)> {
        self.0.iter()
    }
}

impl<T, U> PartialEq for Bijection<T, U>
where
    T: PartialEq + Clone + Ord,
    U: PartialEq + Clone + Ord,
{
    fn eq(&self, other: &Self) -> bool {
        let mut foo = self.0.clone();
        foo.sort();
        foo == other.0
    }
}

impl<T, U> Eq for Bijection<T, U>
where
    T: PartialEq + Clone + Ord,
    U: PartialEq + Clone + Ord,
{
}

impl<T, U> IntoIterator for Bijection<T, U> {
    type Item = (T, U);
    type IntoIter = alloc::vec::IntoIter<(T, U)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
