pub mod meta_tools;
pub mod registry;

use crate::tools::ToolDef;
use std::collections::{HashMap, HashSet};
use tokio::sync::RwLock;

/// Tracks which toolsets are currently loaded in the session.
pub struct ToolRouter {
    /// All registered toolset definitions
    registry: &'static [ToolsetMeta],
    /// Names of currently active toolsets
    active: RwLock<HashSet<String>>,
    /// Flat map of tool_name → ToolDef for fast dispatch
    loaded_tools: RwLock<HashMap<String, ToolDef>>,
}

/// Static metadata for a toolset (not the tools themselves).
#[derive(Debug, Clone)]
pub struct ToolsetMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub tool_count: usize,
}

impl ToolRouter {
    pub fn new() -> Self {
        ToolRouter {
            registry: registry::ALL_TOOLSETS,
            active: RwLock::new(HashSet::new()),
            loaded_tools: RwLock::new(HashMap::new()),
        }
    }

    pub fn all_toolsets(&self) -> &'static [ToolsetMeta] {
        self.registry
    }

    pub async fn load(&self, name: &str) -> Option<Vec<ToolDef>> {
        let defs = registry::tools_for(name)?;
        let mut active = self.active.write().await;
        let mut loaded = self.loaded_tools.write().await;
        active.insert(name.to_string());
        for def in &defs {
            loaded.insert(def.name.to_string(), def.clone());
        }
        Some(defs)
    }

    /// Load the starter kit — a minimal set of toolsets that every session needs.
    ///
    /// This is what runs at server startup. Additional toolsets are loaded on demand
    /// by the LLM calling `load_toolset(name)`. Keeping the baseline small means
    /// `tools/list` costs ~2K tokens instead of ~23K.
    pub async fn load_starter_kit(&self) {
        for name in registry::STARTER_KIT {
            let _ = self.load(name).await;
        }
    }

    /// Load **every** toolset, so the very first `tools/list` is complete.
    ///
    /// This exists for MCP clients that cache the initial tool list and never
    /// re-fetch it on `notifications/tools/list_changed`. For those, a tool
    /// that is not in the first listing can never be called: `load_toolset`
    /// reports the names it loaded but returns no schemas, so the client has
    /// nothing to invoke and `auto_load_toolsets` never gets a chance to fire
    /// — it only helps a caller that already knows the tool name (#134, #169).
    ///
    /// The cost is the whole point of the router: a complete listing is ~25K
    /// tokens per `tools/list` instead of ~2K. Off by default; opt in only if
    /// your client needs it.
    pub async fn load_all(&self) {
        for ts in self.registry {
            let _ = self.load(ts.name).await;
        }
    }

    /// Find which toolset a tool name belongs to, whether or not that toolset
    /// is currently loaded. Used to give the LLM an actionable error when it
    /// calls a tool whose toolset hasn't been loaded yet.
    pub fn find_toolset_for_tool(&self, tool_name: &str) -> Option<&'static str> {
        for ts in self.registry {
            if let Some(defs) = registry::tools_for(ts.name) {
                if defs.iter().any(|d| d.name == tool_name) {
                    return Some(ts.name);
                }
            }
        }
        None
    }

    /// Resolve a tool's definition from the registry whether or not its toolset
    /// is loaded.
    ///
    /// This is what lets `execute_konnect_tool` reach every tool without
    /// mutating the session: loading the toolset would work too, but it would
    /// grow `tools/list` for the rest of the session, which is exactly the
    /// context cost the dispatcher exists to avoid. Resolution is a registry
    /// lookup with no side effects.
    pub fn find_tool_def(&self, tool_name: &str) -> Option<ToolDef> {
        for ts in self.registry {
            if let Some(defs) = registry::tools_for(ts.name) {
                if let Some(def) = defs.into_iter().find(|d| d.name == tool_name) {
                    return Some(def);
                }
            }
        }
        None
    }

    /// Every registered tool as `(toolset, ToolDef)`, for catalogue queries that
    /// must see the whole catalogue rather than the loaded subset.
    pub fn all_tool_defs(&self) -> Vec<(&'static str, ToolDef)> {
        let mut out = Vec::new();
        for ts in self.registry {
            if let Some(defs) = registry::tools_for(ts.name) {
                for def in defs {
                    out.push((ts.name, def));
                }
            }
        }
        out
    }

    pub async fn unload(&self, name: &str) -> bool {
        let defs = match registry::tools_for(name) {
            Some(d) => d,
            None => return false,
        };
        let mut active = self.active.write().await;
        let mut loaded = self.loaded_tools.write().await;
        active.remove(name);
        for def in &defs {
            loaded.remove(def.name);
        }
        true
    }

    pub async fn active_names(&self) -> Vec<String> {
        self.active.read().await.iter().cloned().collect()
    }

    pub async fn get_tool(&self, name: &str) -> Option<ToolDef> {
        self.loaded_tools.read().await.get(name).cloned()
    }

    /// Return all currently active ToolDefs for use in MCP tool listings.
    pub async fn active_tools(&self) -> Vec<ToolDef> {
        self.loaded_tools.read().await.values().cloned().collect()
    }
}

impl Default for ToolRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_toolset_loads() {
        let router = ToolRouter::new();
        for meta in registry::ALL_TOOLSETS {
            assert!(
                router.load(meta.name).await.is_some(),
                "toolset '{}' failed to load",
                meta.name
            );
        }
        assert!(router.load("nonexistent_toolset").await.is_none());
    }

    #[tokio::test]
    async fn starter_kit_loads_expected_toolsets_and_nothing_more() {
        let router = ToolRouter::new();
        router.load_starter_kit().await;
        let active: std::collections::HashSet<String> =
            router.active_names().await.into_iter().collect();
        for expected in registry::STARTER_KIT {
            assert!(
                active.contains(*expected),
                "starter kit missing toolset '{}'",
                expected
            );
        }
        // On-demand toolsets must not be auto-loaded
        assert!(!active.contains("pcb_board"));
        assert!(!active.contains("integration"));
        assert!(!active.contains("templates"));
    }

    /// The eager path exists so a client that never re-fetches `tools/list`
    /// still sees every tool. That only holds if `load_all` really loads all
    /// of them — a partial listing would leave exactly the silent
    /// uncallable-tool failure it is meant to cure (#134, #169).
    #[tokio::test]
    async fn load_all_activates_every_registered_toolset() {
        let router = ToolRouter::new();
        router.load_all().await;
        let active: std::collections::HashSet<String> =
            router.active_names().await.into_iter().collect();
        for ts in registry::ALL_TOOLSETS {
            assert!(active.contains(ts.name), "load_all missed '{}'", ts.name);
        }
        assert_eq!(active.len(), registry::ALL_TOOLSETS.len());

        // And the listing it produces is the full catalogue.
        let listed = router.active_tools().await.len();
        let registered: usize = registry::ALL_TOOLSETS.iter().map(|t| t.tool_count).sum();
        assert_eq!(
            listed, registered,
            "an eager tools/list must carry every registered tool"
        );
    }

    #[tokio::test]
    async fn find_toolset_for_tool_resolves_unloaded_tools() {
        let router = ToolRouter::new();
        router.load_starter_kit().await;
        // pcb_board is NOT in starter kit, but this lookup must still find it
        assert_eq!(
            router.find_toolset_for_tool("place_component"),
            Some("pcb_components")
        );
        assert_eq!(
            router.find_toolset_for_tool("route_trace"),
            Some("pcb_routing")
        );
        assert_eq!(router.find_toolset_for_tool("nonexistent_tool"), None);
    }

    // ─── Registry invariants ─────────────────────────────────────────────────
    //
    // These are the guardrails that protect future work:
    //
    // - The hand-written `tool_count` in ALL_TOOLSETS must match what
    //   `tools_for(name)` actually returns. Otherwise `list_toolboxes` lies
    //   to the LLM.
    // - No toolset grows past ~20 tools. Past that, split it — 20 tool
    //   descriptions at ~400 bytes each is already a 1.6KB payload in
    //   `tools/list` when loaded, and tool selection accuracy degrades.

    /// The cap above which a toolset must be split. If you hit this, either
    /// move tools to a sibling toolset or add a new one — don't raise this
    /// number without a conversation.
    ///
    /// Raised 20 → 22 when `library` reached 21 with `set_symbol_graphics` and
    /// `add_symbol_text`. The cap was written against a payload cost that the
    /// dispatcher has since changed: `tools/list` no longer carries a loaded
    /// toolset's schemas for a client using `execute_konnect_tool`, so the
    /// "1.6KB in every listing" argument no longer applies the same way. The
    /// second half of the rationale — that tool-selection accuracy degrades as
    /// a toolset grows — is unaffected by that and is why this is 22 rather
    /// than removed. `library` is the only toolset near it; a third symbol or
    /// footprint editor is the point to split authoring from registration
    /// rather than raise this again.
    const MAX_TOOLS_PER_TOOLSET: usize = 22;

    #[test]
    fn registry_tool_counts_match_reality() {
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name)
                .unwrap_or_else(|| panic!("tools_for({}) returned None", meta.name));
            assert_eq!(
                defs.len(),
                meta.tool_count,
                "registry declares tool_count={} for '{}' but tools_for() returned {} tools — \
                 update ALL_TOOLSETS in router/registry.rs",
                meta.tool_count,
                meta.name,
                defs.len()
            );
        }
    }

    #[test]
    fn no_toolset_has_duplicate_tool_names() {
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name).unwrap();
            let mut seen = std::collections::HashSet::new();
            for d in &defs {
                assert!(
                    seen.insert(d.name),
                    "duplicate tool name '{}' inside toolset '{}'",
                    d.name,
                    meta.name
                );
            }
        }
    }

    #[test]
    fn tool_names_unique_across_toolsets() {
        // Duplicate names across toolsets are a silent foot-gun: whichever
        // toolset is loaded last wins in `loaded_tools`, so behavior depends
        // on load order. Aliases that point at the same handler are fine; the
        // test fails on first occurrence so the committer has to decide.
        let mut owner: std::collections::HashMap<&'static str, &'static str> =
            std::collections::HashMap::new();
        let mut collisions = Vec::new();
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name).unwrap();
            for d in &defs {
                if let Some(prev) = owner.insert(d.name, meta.name) {
                    if prev != meta.name {
                        collisions.push(format!(
                            "'{}' declared in both '{}' and '{}'",
                            d.name, prev, meta.name
                        ));
                    }
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "tool name collisions across toolsets (last-loaded wins in the router):\n  {}",
            collisions.join("\n  ")
        );
    }

    #[test]
    fn no_toolset_exceeds_max_size() {
        for meta in registry::ALL_TOOLSETS {
            assert!(
                meta.tool_count <= MAX_TOOLS_PER_TOOLSET,
                "toolset '{}' has {} tools, which exceeds the soft cap of {}. \
                 Split it into two before bumping this cap.",
                meta.name,
                meta.tool_count,
                MAX_TOOLS_PER_TOOLSET
            );
        }
    }

    #[test]
    fn starter_kit_entries_are_all_valid_toolsets() {
        for name in registry::STARTER_KIT {
            assert!(
                registry::tools_for(name).is_some(),
                "STARTER_KIT references unknown toolset '{}'",
                name
            );
        }
    }
}
