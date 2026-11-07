use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibleRow {
    pub id: i64,
    pub index: usize,
    pub top_px: f32,
    pub height_px: f32,
}

#[derive(Debug)]
pub struct StableList {
    ids: Vec<i64>,
    heights: HashMap<i64, f32>,
    offsets: Vec<f32>,
    estimated_px: f32,
}

impl StableList {
    pub fn new(estimated_px: f32) -> Self {
        Self {
            ids: Vec::new(),
            heights: HashMap::new(),
            offsets: vec![0.],
            estimated_px: estimated_px.max(1.),
        }
    }

    pub fn set_items(&mut self, ids: &[i64]) {
        self.ids.clear();
        self.ids.extend_from_slice(ids);
        let current: HashSet<_> = ids.iter().copied().collect();
        self.heights.retain(|id, _| current.contains(id));
        self.rebuild();
    }

    pub fn measure(&mut self, id: i64, height_px: f32) {
        if !height_px.is_finite() || height_px <= 0. || !self.ids.contains(&id) {
            return;
        }
        if self
            .heights
            .get(&id)
            .is_some_and(|old| (old - height_px).abs() < 0.5)
        {
            return;
        }
        self.heights.insert(id, height_px);
        self.rebuild();
    }

    pub fn total_height(&self) -> f32 {
        *self.offsets.last().unwrap_or(&0.)
    }

    pub fn visible(&self, offset_px: f32, viewport_px: f32, overscan_px: f32) -> Vec<VisibleRow> {
        let start = (offset_px - overscan_px.max(0.)).max(0.);
        let end = offset_px.max(0.) + viewport_px.max(0.) + overscan_px.max(0.);
        let first = self
            .offsets
            .partition_point(|&top| top <= start)
            .saturating_sub(1)
            .min(self.ids.len());
        (first..self.ids.len())
            .take_while(|&index| self.offsets[index] < end)
            .map(|index| VisibleRow {
                id: self.ids[index],
                index,
                top_px: self.offsets[index],
                height_px: self.offsets[index + 1] - self.offsets[index],
            })
            .collect()
    }

    pub fn anchor(&self, offset_px: f32) -> Option<(i64, f32)> {
        if self.ids.is_empty() {
            return None;
        }
        let index = self
            .offsets
            .partition_point(|&top| top <= offset_px.max(0.))
            .saturating_sub(1)
            .min(self.ids.len() - 1);
        Some((self.ids[index], offset_px.max(0.) - self.offsets[index]))
    }

    pub fn offset_for_anchor(&self, id: i64, inset_px: f32) -> Option<f32> {
        self.ids
            .iter()
            .position(|candidate| *candidate == id)
            .map(|index| self.offsets[index] + inset_px)
    }

    fn rebuild(&mut self) {
        self.offsets.clear();
        self.offsets.push(0.);
        for id in &self.ids {
            self.offsets.push(
                self.offsets.last().copied().unwrap_or(0.)
                    + self.heights.get(id).copied().unwrap_or(self.estimated_px),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_identity_and_anchor_survive_reorder_and_height_change() {
        let mut list = StableList::new(60.);
        list.set_items(&[11, 22, 33]);
        list.measure(22, 120.);
        assert_eq!(
            list.visible(50., 100., 0.)
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![11, 22]
        );
        let anchor = list.anchor(80.).unwrap();
        assert_eq!(anchor, (22, 20.));
        list.set_items(&[33, 11, 22]);
        assert_eq!(list.offset_for_anchor(anchor.0, anchor.1), Some(140.));
        assert_eq!(list.total_height(), 240.);
    }

    #[test]
    fn rejects_invalid_measurement() {
        let mut list = StableList::new(44.);
        list.set_items(&[1]);
        list.measure(1, f32::NAN);
        list.measure(1, 0.);
        assert_eq!(list.total_height(), 44.);
    }
}
