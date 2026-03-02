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
