use anyhow::Result;
use rustc_hash::FxHashSet;
use turbo_tasks::{ResolvedVc, TryJoinIterExt, Vc};

use crate::{
    module::Module,
    module_graph::{GraphTraversalAction, ModuleGraph, SingleModuleGraphWithBindingUsage},
};

/// This lists all the modules that are side effect free
/// This means they are either declared side effect free by some configuration or they have been
/// determined to be side effect free via static analysis of the module evaluation and dependencies.
#[turbo_tasks::value(transparent)]
pub struct SideEffectFreeModules(FxHashSet<ResolvedVc<Box<dyn Module>>>);

#[turbo_tasks::function(operation)]
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
    let parent_async_modules = parent_side_effect_free_modules.await?;
    let graph = graph.read().await?;

    let self_async_modules = graph
        .graphs
        .first()
        .unwrap()
        .iter_nodes()
        .map(async |node| Ok((node, *node.is_self_async().await?)))
        .try_join()
        .await?
        .into_iter()
        .flat_map(|(k, v)| v.then_some(k))
        .chain(parent_async_modules.iter().copied())
        .collect::<FxHashSet<_>>();

    // To determine which modules are async, we need to propagate the self-async flag to all
    // importers, which is done using a postorder traversal of the graph.
    //
    // This however doesn't cover cycles of async modules, which are handled by determining all
    // strongly-connected components, and then marking all the whole SCC as async if one of the
    // modules in the SCC is async.

    let mut async_modules = self_async_modules;
    graph.traverse_edges_from_entries_dfs(
        graph.graphs.first().unwrap().entry_modules(),
        &mut (),
        |_, _, _| Ok(GraphTraversalAction::Continue),
        |parent_info, module, _| {
            let Some((parent_module, ref_data)) = parent_info else {
                // An entry module
                return Ok(());
            };

            if ref_data.chunking_type.is_inherit_async() && async_modules.contains(&module) {
                async_modules.insert(parent_module);
            }
            Ok(())
        },
    )?;

    graph.traverse_cycles(
        |ref_data| ref_data.chunking_type.is_inherit_async(),
        |cycle| {
            if cycle.iter().any(|node| async_modules.contains(node)) {
                async_modules.extend(cycle.iter().map(|n| **n));
            }
            Ok(())
        },
    )?;

    Ok(Vc::cell(async_modules))
}
