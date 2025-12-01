use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use turbo_tasks::{ResolvedVc, TryJoinIterExt, Vc};

use crate::{
    module::{Module, ModuleSideEffects},
    module_graph::{GraphTraversalAction, ModuleGraph, SingleModuleGraphWithBindingUsage},
};

/// This lists all the modules that are side effect free
/// This means they are either declared side effect free by some configuration or they have been
/// determined to be side effect free via static analysis of the module evaluation and dependencies.
#[turbo_tasks::value(transparent)]
pub struct SideEffectFreeModules(FxHashSet<ResolvedVc<Box<dyn Module>>>);

#[turbo_tasks::function]
pub async fn compute_side_effect_free_module_info(
    graphs: ResolvedVc<ModuleGraph>,
) -> Result<Vc<SideEffectFreeModules>> {
    // Layout segment optimization, we can individually compute the side effect free modules for
    // each graph.
    let mut result: Vc<SideEffectFreeModules> = Vc::cell(Default::default());
    let graphs = graphs.await?;
    for graph in graphs.iter_graphs() {
        result = compute_side_effect_free_module_info_single(graph, result);
    }
    Ok(result)
}

#[turbo_tasks::function]
async fn compute_side_effect_free_module_info_single(
    graph: SingleModuleGraphWithBindingUsage,
    parent_side_effect_free_modules: Vc<SideEffectFreeModules>,
) -> Result<Vc<SideEffectFreeModules>> {
    let parent_side_effect_free_modules = parent_side_effect_free_modules.await?;
    let graph = graph.read().await?;

    let module_side_effects = graph
        .enumerate_nodes()
        .map(async |(_, node)| {
            Ok(match node {
                super::SingleModuleGraphNode::Module(module) => {
                    // This turbo task always has a cache hit since it is called when building the
                    // module graph. we could consider moving this information
                    // into to the module graph, but then changes would invalidate the whole graph.
                    (*module, *module.side_effects().await?)
                }
                super::SingleModuleGraphNode::VisitedModule { idx: _, module } => (
                    *module,
                    if parent_side_effect_free_modules.contains(module) {
                        ModuleSideEffects::SideEffectFree
                    } else {
                        ModuleSideEffects::SideEffectful
                    },
                ),
            })
        })
        .try_join()
        .await?
        .into_iter()
        .collect::<FxHashMap<_, _>>();

    // Modules are categorized as side-effectful, locally side effect free and side effect free.
    // So we are really just interested in determining what modules that are locally side effect
    // free. logically we want to start at all such modules are determine if their transitive
    // dependencies are side effect free.

    let mut locally_side_effect_free_modules_that_have_side_effects = FxHashSet::default();
    graph.traverse_edges_from_entries_dfs_reversed(
        // Start from all the side effectful nodes
        module_side_effects.iter().filter_map(|(m, e)| {
            if *e == ModuleSideEffects::SideEffectful {
                Some(*m)
            } else {
                None
            }
        }),
        &mut (),
        // child is a previously visited module that we know is side effectful
        |child, _parent, _s| {
            Ok(if let Some((child_module, _edge)) = child {
                match module_side_effects.get(&child_module).unwrap() {
                    ModuleSideEffects::SideEffectful | ModuleSideEffects::SideEffectFree => {
                        // We have either already seen this or don't want to follow it
                        GraphTraversalAction::Exclude
                    }
                    ModuleSideEffects::ModuleEvaluationIsSideEffectFree => {
                        // this module is side effect free locally but must depend on something
                        // effectful so it to is effectful
                        locally_side_effect_free_modules_that_have_side_effects
                            .insert(child_module);
                        GraphTraversalAction::Continue
                    }
                }
            } else {
                // entry point, keep going
                GraphTraversalAction::Continue
            })
        },
        |_, _, _| Ok(()),
    )?;
    let side_effect_free_modules = module_side_effects
        .into_iter()
        .filter_map(|(m, e)| match e {
            ModuleSideEffects::SideEffectful => None,
            ModuleSideEffects::SideEffectFree => Some(m),
            ModuleSideEffects::ModuleEvaluationIsSideEffectFree => {
                if locally_side_effect_free_modules_that_have_side_effects.contains(&m) {
                    None
                } else {
                    Some(m)
                }
            }
        })
        .chain(parent_side_effect_free_modules.iter().copied())
        .collect::<FxHashSet<_>>();

    Ok(Vc::cell(side_effect_free_modules))
}
