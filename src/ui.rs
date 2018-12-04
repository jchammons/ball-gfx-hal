pub use imgui::{ImStr, Ui};

pub fn enum_combo<'p, 't, 'ui, T: PartialEq + Eq + Copy>(
    ui: &Ui<'ui>,
    label: &'p ImStr,
    current: &mut T,
    labels: &[&'p ImStr],
    variants: &[T],
    height_in_items: usize,
) -> bool {
    // Determine index of current
    let mut idx = variants
        .iter()
        .enumerate()
        .find(|(_, &variant)| variant == *current)
        .expect("`current' is not a listed variant")
        .0 as i32;

    let changed = ui.combo(label, &mut idx, labels, height_in_items as i32);
    if changed {
        *current = variants[idx as usize];
    }
    changed
}
