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
        Self(Vec::new())
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

    pub fn as_slice(&self) -> &[(T, U)] {
        self.0.as_slice()
    }
}

impl<T, U> PartialEq for Bijection<T, U>
where
    T: PartialEq + Clone + Ord,
    U: PartialEq + Clone + Ord,
{
    fn eq(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        let mut a = self.0.clone();
        let mut b = other.0.clone();
        a.sort();
        b.sort();
        a == b
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

impl<T, U> Default for Bijection<T, U> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn partial_eq_is_reflexive_and_order_independent() {
        let a = Bijection(vec![(2u32, 20u32), (1, 10)]);
        let b = Bijection(vec![(1u32, 10u32), (2, 20)]);
        assert_eq!(a, a);
        assert_eq!(a, b);
        assert_eq!(b, a);
    }
}
