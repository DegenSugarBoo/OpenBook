use eframe::egui;
use egui_tiles::{Container, SimplificationOptions, Tile, TileId, Tiles, Tree, UiResponse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const LAYOUT_PREFS_KEY: &str = "layout_prefs";
pub const LAYOUT_STORE_V1_KEY: &str = "layout_store_v1";
pub const LAYOUT_STORE_V2_KEY: &str = "layout_store_v2";
pub const LAYOUT_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_LAYOUT_PROFILE_NAME: &str = "Default";
pub const MAX_LAYOUT_PROFILE_NAME_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaneKind {
    Heatmap,
    OrderBook,
    MarketImpact,
    FillKill,
    TradesTape,
}

impl PaneKind {
    pub const ALL: [Self; 5] = [
        Self::Heatmap,
        Self::OrderBook,
        Self::MarketImpact,
        Self::FillKill,
        Self::TradesTape,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Heatmap => "Heatmap",
            Self::OrderBook => "Order Book",
            Self::MarketImpact => "Market Impact",
            Self::FillKill => "Fill:Kill",
            Self::TradesTape => "Trades Tape",
        }
    }

    pub const fn id_str(self) -> &'static str {
        match self {
            Self::Heatmap => "heatmap",
            Self::OrderBook => "order_book",
            Self::MarketImpact => "market_impact",
            Self::FillKill => "fill_kill",
            Self::TradesTape => "trades_tape",
        }
    }
}

pub trait PaneRenderer {
    fn render_pane(&mut self, ui: &mut egui::Ui, pane: PaneKind);
}

pub struct WorkspaceBehavior<'a, R: PaneRenderer> {
    pub renderer: &'a mut R,
    pub edited: &'a mut bool,
}

impl<R: PaneRenderer> egui_tiles::Behavior<PaneKind> for WorkspaceBehavior<'_, R> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile_id: TileId, pane: &mut PaneKind) -> UiResponse {
        self.renderer.render_pane(ui, *pane);
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &PaneKind) -> egui::WidgetText {
        pane.title().into()
    }

    fn min_size(&self) -> f32 {
        48.0
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        4.0
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            all_panes_must_have_tabs: false,
            join_nested_linear_containers: true,
            ..SimplificationOptions::default()
        }
    }

    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        24.0
    }

    fn is_tab_closable(&self, _tiles: &Tiles<PaneKind>, _tile_id: TileId) -> bool {
        false
    }

    fn on_edit(&mut self, _edit_action: egui_tiles::EditAction) {
        *self.edited = true;
    }
}

pub fn build_default_tree() -> Tree<PaneKind> {
    let mut tiles = Tiles::default();

    let heatmap = tiles.insert_pane(PaneKind::Heatmap);
    let order_book = tiles.insert_pane(PaneKind::OrderBook);
    let market_impact = tiles.insert_pane(PaneKind::MarketImpact);
    let fill_kill = tiles.insert_pane(PaneKind::FillKill);
    let trades_tape = tiles.insert_pane(PaneKind::TradesTape);

    let bottom = tiles.insert_horizontal_tile(vec![trades_tape, fill_kill]);
    set_linear_shares(
        &mut tiles,
        bottom,
        &[(trades_tape, 0.60), (fill_kill, 0.40)],
    );

    let left = tiles.insert_vertical_tile(vec![heatmap, bottom]);
    set_linear_shares(&mut tiles, left, &[(heatmap, 0.72), (bottom, 0.28)]);

    let right = tiles.insert_vertical_tile(vec![order_book, market_impact]);
    set_linear_shares(
        &mut tiles,
        right,
        &[(order_book, 0.65), (market_impact, 0.35)],
    );

    let root = tiles.insert_horizontal_tile(vec![left, right]);
    set_linear_shares(&mut tiles, root, &[(left, 0.74), (right, 0.26)]);

    Tree::new("workspace", root, tiles)
}

fn set_linear_shares(tiles: &mut Tiles<PaneKind>, tile_id: TileId, shares: &[(TileId, f32)]) {
    if let Some(Tile::Container(Container::Linear(linear))) = tiles.get_mut(tile_id) {
        for &(child_id, share) in shares {
            linear.shares.set_share(child_id, share);
        }
    }
}

pub fn pane_tile_map(tree: &Tree<PaneKind>) -> HashMap<PaneKind, TileId> {
    let mut ids = HashMap::with_capacity(PaneKind::ALL.len());
    for (tile_id, tile) in tree.tiles.iter() {
        if let Tile::Pane(pane) = tile {
            ids.insert(*pane, *tile_id);
        }
    }
    ids
}

pub fn ensure_all_panes(tree: &mut Tree<PaneKind>) -> HashMap<PaneKind, TileId> {
    if tree.root.is_none() {
        *tree = build_default_tree();
        return pane_tile_map(tree);
    }

    let mut ids = pane_tile_map(tree);
    for pane in PaneKind::ALL {
        if ids.contains_key(&pane) {
            continue;
        }

        let new_id = tree.tiles.insert_pane(pane);
        ids.insert(pane, new_id);
        if let Some(root) = tree.root {
            if let Some(Tile::Container(container)) = tree.tiles.get_mut(root) {
                container.add_child(new_id);
            } else {
                let new_root = tree.tiles.insert_horizontal_tile(vec![root, new_id]);
                tree.root = Some(new_root);
            }
        } else {
            tree.root = Some(new_id);
        }
    }

    ids
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyLayoutPrefs {
    pub show_heatmap_window: bool,
    pub show_order_book_window: bool,
    pub show_impact_window: bool,
    pub show_fill_kill_window: bool,
    pub show_trades_tape_window: bool,
}

impl Default for LegacyLayoutPrefs {
    fn default() -> Self {
        Self {
            show_heatmap_window: true,
            show_order_book_window: true,
            show_impact_window: true,
            show_fill_kill_window: true,
            show_trades_tape_window: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyLayoutProfile {
    pub name: String,
    pub prefs: LegacyLayoutPrefs,
}

impl Default for LegacyLayoutProfile {
    fn default() -> Self {
        Self {
            name: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
            prefs: LegacyLayoutPrefs::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyLayoutStore {
    pub schema_version: u32,
    pub active_profile: String,
    pub profiles: Vec<LegacyLayoutProfile>,
}

impl Default for LegacyLayoutStore {
    fn default() -> Self {
        Self {
            schema_version: 1,
            active_profile: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
            profiles: vec![LegacyLayoutProfile::default()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutProfileV2 {
    pub name: String,
    pub dock_tree: Tree<PaneKind>,
}

impl Default for LayoutProfileV2 {
    fn default() -> Self {
        Self {
            name: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
            dock_tree: build_default_tree(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutStoreV2 {
    pub schema_version: u32,
    pub active_profile: String,
    pub profiles: Vec<LayoutProfileV2>,
}

impl Default for LayoutStoreV2 {
    fn default() -> Self {
        Self {
            schema_version: LAYOUT_SCHEMA_VERSION,
            active_profile: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
            profiles: vec![LayoutProfileV2::default()],
        }
    }
}

impl LayoutStoreV2 {
    pub fn active_index(&self) -> usize {
        self.profiles
            .iter()
            .position(|profile| profile.name.eq_ignore_ascii_case(&self.active_profile))
            .unwrap_or(0)
    }

    pub fn set_active_index(&mut self, idx: usize) {
        if let Some(profile) = self.profiles.get(idx) {
            self.active_profile = profile.name.clone();
        }
    }

    pub fn create_profile(
        &mut self,
        name: &str,
        dock_tree: Tree<PaneKind>,
    ) -> Result<usize, String> {
        let normalized = validate_layout_profile_name(name, &self.profiles, None)?;
        self.profiles.push(LayoutProfileV2 {
            name: normalized,
            dock_tree,
        });
        let idx = self.profiles.len().saturating_sub(1);
        self.set_active_index(idx);
        Ok(idx)
    }

    pub fn rename_profile(&mut self, idx: usize, new_name: &str) -> Result<(), String> {
        if idx >= self.profiles.len() {
            return Err("Profile does not exist.".to_string());
        }
        let was_active = self.profiles[idx]
            .name
            .eq_ignore_ascii_case(&self.active_profile);
        let normalized = validate_layout_profile_name(new_name, &self.profiles, Some(idx))?;
        self.profiles[idx].name = normalized;
        if was_active {
            self.active_profile = self.profiles[idx].name.clone();
        } else {
            self.set_active_index(self.active_index());
        }
        Ok(())
    }

    pub fn delete_profile(&mut self, idx: usize) -> Result<bool, String> {
        if self.profiles.len() <= 1 {
            return Err("Cannot delete the last remaining profile.".to_string());
        }
        if idx >= self.profiles.len() {
            return Err("Profile does not exist.".to_string());
        }
        let deleted_was_active = self.profiles[idx]
            .name
            .eq_ignore_ascii_case(&self.active_profile);
        self.profiles.remove(idx);

        if deleted_was_active {
            let fallback = self
                .profiles
                .iter()
                .position(|profile| {
                    profile
                        .name
                        .eq_ignore_ascii_case(DEFAULT_LAYOUT_PROFILE_NAME)
                })
                .unwrap_or(0);
            self.set_active_index(fallback);
        } else {
            self.set_active_index(self.active_index());
        }

        Ok(deleted_was_active)
    }
}

pub fn normalize_layout_profile_name(input: &str) -> String {
    let trimmed = input.trim();
    trimmed
        .chars()
        .take(MAX_LAYOUT_PROFILE_NAME_LEN)
        .collect::<String>()
}

pub fn validate_layout_profile_name(
    input: &str,
    profiles: &[LayoutProfileV2],
    skip_idx: Option<usize>,
) -> Result<String, String> {
    let normalized = normalize_layout_profile_name(input);
    if normalized.is_empty() {
        return Err("Profile name cannot be empty.".to_string());
    }
    if input.trim().chars().count() > MAX_LAYOUT_PROFILE_NAME_LEN {
        return Err(format!(
            "Profile name must be at most {} characters.",
            MAX_LAYOUT_PROFILE_NAME_LEN
        ));
    }
    let duplicate = profiles.iter().enumerate().any(|(idx, profile)| {
        Some(idx) != skip_idx && profile.name.eq_ignore_ascii_case(&normalized)
    });
    if duplicate {
        return Err("Profile name already exists.".to_string());
    }
    Ok(normalized)
}

pub fn sanitize_layout_store(mut store: LayoutStoreV2) -> LayoutStoreV2 {
    store.schema_version = LAYOUT_SCHEMA_VERSION;
    if store.profiles.is_empty() {
        return LayoutStoreV2::default();
    }

    let mut used_names: Vec<String> = Vec::with_capacity(store.profiles.len());
    for (idx, profile) in store.profiles.iter_mut().enumerate() {
        let mut candidate = normalize_layout_profile_name(&profile.name);
        if candidate.is_empty() {
            candidate = if idx == 0 {
                DEFAULT_LAYOUT_PROFILE_NAME.to_string()
            } else {
                format!("Profile {}", idx + 1)
            };
        }

        let mut suffix = 2_usize;
        let base = candidate.clone();
        while used_names
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(&candidate))
        {
            let suffix_text = format!(" {}", suffix);
            let keep = MAX_LAYOUT_PROFILE_NAME_LEN.saturating_sub(suffix_text.len());
            candidate = format!(
                "{}{}",
                base.chars().take(keep).collect::<String>(),
                suffix_text
            );
            suffix += 1;
        }

        profile.name = candidate.clone();
        ensure_all_panes(&mut profile.dock_tree);
        used_names.push(candidate);
    }

    let active_idx = store
        .profiles
        .iter()
        .position(|profile| profile.name.eq_ignore_ascii_case(&store.active_profile))
        .unwrap_or(0);
    store.active_profile = store.profiles[active_idx].name.clone();
    store
}

pub fn tree_from_legacy_prefs(prefs: &LegacyLayoutPrefs) -> Tree<PaneKind> {
    let mut tree = build_default_tree();
    let pane_ids = pane_tile_map(&tree);
    let visibility = [
        (PaneKind::Heatmap, prefs.show_heatmap_window),
        (PaneKind::OrderBook, prefs.show_order_book_window),
        (PaneKind::MarketImpact, prefs.show_impact_window),
        (PaneKind::FillKill, prefs.show_fill_kill_window),
        (PaneKind::TradesTape, prefs.show_trades_tape_window),
    ];
    for (pane, visible) in visibility {
        if let Some(tile_id) = pane_ids.get(&pane).copied() {
            tree.tiles.set_visible(tile_id, visible);
        }
    }
    tree
}

pub fn migrate_v1_store(store: LegacyLayoutStore) -> LayoutStoreV2 {
    let mut migrated = LayoutStoreV2 {
        schema_version: LAYOUT_SCHEMA_VERSION,
        active_profile: store.active_profile,
        profiles: store
            .profiles
            .into_iter()
            .map(|profile| LayoutProfileV2 {
                name: profile.name,
                dock_tree: tree_from_legacy_prefs(&profile.prefs),
            })
            .collect(),
    };
    if migrated.profiles.is_empty() {
        migrated = LayoutStoreV2::default();
    }
    sanitize_layout_store(migrated)
}

pub fn migrate_legacy_prefs(prefs: LegacyLayoutPrefs) -> LayoutStoreV2 {
    let profile = LayoutProfileV2 {
        name: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
        dock_tree: tree_from_legacy_prefs(&prefs),
    };
    sanitize_layout_store(LayoutStoreV2 {
        schema_version: LAYOUT_SCHEMA_VERSION,
        active_profile: DEFAULT_LAYOUT_PROFILE_NAME.to_string(),
        profiles: vec![profile],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tree_contains_all_panes() {
        let tree = build_default_tree();
        let ids = pane_tile_map(&tree);
        for pane in PaneKind::ALL {
            assert!(ids.contains_key(&pane), "missing pane: {:?}", pane);
        }
    }

    #[test]
    fn migrate_from_legacy_layout_store_keeps_visibility() {
        let store = LegacyLayoutStore {
            schema_version: 1,
            active_profile: "Default".to_string(),
            profiles: vec![LegacyLayoutProfile {
                name: "Default".to_string(),
                prefs: LegacyLayoutPrefs {
                    show_heatmap_window: false,
                    show_order_book_window: true,
                    show_impact_window: false,
                    show_fill_kill_window: true,
                    show_trades_tape_window: false,
                },
            }],
        };

        let migrated = migrate_v1_store(store);
        let mut tree = migrated.profiles[0].dock_tree.clone();
        let pane_ids = ensure_all_panes(&mut tree);
        assert!(!tree.tiles.is_visible(pane_ids[&PaneKind::Heatmap]));
        assert!(tree.tiles.is_visible(pane_ids[&PaneKind::OrderBook]));
        assert!(!tree.tiles.is_visible(pane_ids[&PaneKind::MarketImpact]));
        assert!(tree.tiles.is_visible(pane_ids[&PaneKind::FillKill]));
        assert!(!tree.tiles.is_visible(pane_ids[&PaneKind::TradesTape]));
    }

    #[test]
    fn cannot_delete_last_profile() {
        let mut store = LayoutStoreV2::default();
        let result = store.delete_profile(0);
        assert!(result.is_err());
    }
}
