use cairo_lang_defs::ids::MemberId;
use cairo_lang_proc_macros::DebugWithDb;
use cairo_lang_semantic::expr::fmt::ExprFormatter;
use cairo_lang_semantic::usage::MemberPath;
use cairo_lang_semantic::{self as semantic};
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::{extract_matches, try_extract_matches};
use itertools::{Itertools, chain};

use crate::VariableId;
use crate::db::LoweringGroup;

/// Information about members captured by the closure and their types.
#[derive(Clone, Debug)]
pub struct ClosureInfo {
    // TODO(TomerStarkware): unite copiable members and snapshots into a single map.
    /// The members captured by the closure (not as snapshot).
    pub members: OrderedHashMap<MemberPath, semantic::TypeId>,
    /// The types of the captured snapshot variables.
    pub snapshots: OrderedHashMap<MemberPath, semantic::TypeId>,
}

#[derive(Clone, Default, Debug)]
pub struct SemanticLoweringMapping {
    /// Maps member paths ([MemberPath]) to lowered variable ids or scattered variable ids.
    scattered: OrderedHashMap<MemberPath, Value>,
}
impl SemanticLoweringMapping {
    /// Returns the topmost mapped member path containing the given member path, or None no such
    /// member path exists in the mapping.
    pub fn topmost_mapped_containing_member_path(
        &self,
        mut member_path: MemberPath,
    ) -> Option<MemberPath> {
        let mut res = None;
        loop {
            if self.scattered.contains_key(&member_path) {
                res = Some(member_path.clone());
            }
            let MemberPath::Member { parent, .. } = member_path else {
                return res;
            };
            member_path = *parent;
        }
    }

    /// Returns the scattered members of the given member path, or None if the member path is not
    /// scattered.
    pub fn get_scattered_members(&self, member_path: &MemberPath) -> Option<Vec<MemberPath>> {
        let Some(Value::Scattered(scattered)) = self.scattered.get(member_path) else {
            return None;
        };
        Some(
            scattered
                .members
                .iter()
                .map(|(member_id, _)| MemberPath::Member {
                    parent: member_path.clone().into(),
                    member_id: *member_id,
                    concrete_struct_id: scattered.concrete_struct_id,
                })
                .collect(),
        )
    }

    pub fn destructure_closure<TContext: StructRecomposer>(
        &mut self,
        ctx: &mut TContext,
        closure_var: VariableId,
        closure_info: &ClosureInfo,
    ) -> Vec<VariableId> {
        ctx.deconstruct_by_types(
            closure_var,
            chain!(closure_info.members.values(), closure_info.snapshots.values()).cloned(),
        )
    }

    pub fn get<TContext: StructRecomposer>(
        &mut self,
        mut ctx: TContext,
        path: &MemberPath,
    ) -> Option<VariableId> {
        let value = self.break_into_value(&mut ctx, path)?;
        Self::assemble_value(&mut ctx, value)
    }

    pub fn introduce(&mut self, path: MemberPath, var: VariableId) {
        self.scattered.insert(path, Value::Var(var));
    }

    pub fn update<TContext: StructRecomposer>(
        &mut self,
        ctx: &mut TContext,
        path: &MemberPath,
        var: VariableId,
    ) -> Option<()> {
        // TODO(TomerStarkware): check if path is captured by a closure and invalidate the closure.
        // Right now this can only happen if we take a snapshot of the variable (as the
        // snapshot function returns a new var).
        // we need to ensure the borrow checker invalidates the closure when mutable capture
        // is supported.

        let value = self.break_into_value(ctx, path)?;
        *value = Value::Var(var);
        Some(())
    }

    fn assemble_value<TContext: StructRecomposer>(
        ctx: &mut TContext,
        value: &mut Value,
    ) -> Option<VariableId> {
        Some(match value {
            Value::Var(var) => *var,
            Value::Scattered(scattered) => {
                let members = scattered
                    .members
                    .iter_mut()
                    .map(|(_, value)| Self::assemble_value(ctx, value))
                    .collect::<Option<_>>()?;
                let var = ctx.reconstruct(scattered.concrete_struct_id, members);
                *value = Value::Var(var);
                var
            }
        })
    }

    fn break_into_value<TContext: StructRecomposer>(
        &mut self,
        ctx: &mut TContext,
        path: &MemberPath,
    ) -> Option<&mut Value> {
        if self.scattered.contains_key(path) {
            return self.scattered.get_mut(path);
        }

        let MemberPath::Member { parent, member_id, concrete_struct_id, .. } = path else {
            return None;
        };

        let parent_value = self.break_into_value(ctx, parent)?;
        match parent_value {
            Value::Var(var) => {
                let members = ctx.deconstruct(*concrete_struct_id, *var);
                let members = OrderedHashMap::from_iter(
                    members.into_iter().map(|(member_id, var)| (member_id, Value::Var(var))),
                );
                let scattered = Scattered { concrete_struct_id: *concrete_struct_id, members };
                *parent_value = Value::Scattered(Box::new(scattered));

                extract_matches!(parent_value, Value::Scattered).members.get_mut(member_id)
            }
            Value::Scattered(scattered) => scattered.members.get_mut(member_id),
        }
    }
}

impl<'a> cairo_lang_debug::debug::DebugWithDb<ExprFormatter<'a>> for SemanticLoweringMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>, db: &ExprFormatter<'a>) -> std::fmt::Result {
        for (member_path, value) in self.scattered.iter() {
            writeln!(f, "{:?}: {value}", member_path.debug(db))?;
        }
        Ok(())
    }
}

/// Merges [SemanticLoweringMapping] from multiple blocks to a single [SemanticLoweringMapping].
///
/// The mapping from semantic variables to lowered variables in the new block follows these rules:
///
/// * Variables mapped to the same lowered variable across all input blocks are kept as-is.
/// * Local variables that appear in only a subset of the blocks are removed.
/// * Variables with different mappings across blocks are remapped to a new lowered variable, by
///   invoking the `remapped_callback` function.
pub fn merge_semantics<'a>(
    mappings: impl Iterator<Item = &'a SemanticLoweringMapping>,
    remapped_callback: &mut impl FnMut(&MemberPath) -> VariableId,
) -> SemanticLoweringMapping {
    // A map from [MemberPath] to its [Value] in the `mappings` where it appears.
    // If the number of [Value]s is not the length of `mappings`, it is later dropped.
    let mut path_to_values: OrderedHashMap<MemberPath, Vec<Value>> = Default::default();

    let mut n_mappings = 0;
    for map in mappings {
        for (path, var) in map.scattered.iter() {
            path_to_values.entry(path.clone()).or_default().push(var.clone());
        }
        n_mappings += 1;
    }

    let mut scattered: OrderedHashMap<MemberPath, Value> = Default::default();
    for (path, values) in path_to_values {
        // The variable is missing in one or more of the maps.
        // It cannot be used in the merged block.
        if values.len() != n_mappings {
            continue;
        }

        let merged_value = compute_remapped_variables(
            &values.iter().collect_vec(),
            false,
            &path,
            remapped_callback,
        );
        scattered.insert(path, merged_value);
    }

    SemanticLoweringMapping { scattered }
}

/// Given a list of [Value]s that correspond to the same semantic [MemberPath] in different blocks,
/// compute the [Value] in the merge block.
///
/// If all values are the same, no remapping is needed.
/// If some of the values are [Value::Var] and some are [Value::Scattered], then all the values
/// inside the [Value::Scattered] values need to be remapped.
/// If all of them are [Value::Scattered], then it is possible that some of the members require
/// remapping and some don't.
///
/// Pass `require_remapping=true` to indicate that during the recursion we encountered a
/// [Value::Var], and thus we need to remap all the [Value::Scattered] values.
/// In particular, once we have `require_remapping=true`, all the recursive calls in the subtree
/// will have `require_remapping=true`.
///
/// For example, suppose `values` consists of two trees:
/// * `A = Scattered(Scattered(v0, v1), v2)` and
/// * `B = Scattered(Scattered(v0, v3), v4)`.
///
/// Then, the result will be:
/// * `Scattered(Scattered(v0, new_var), new_var)`,
///
/// since `v0` is the same in both trees, but the other nodes are not.
///
/// If in addition to `A` and `B`, we have another tree
/// * `C = Scattered(v5, v6)`,
///
/// then `v5` will need to be deconstructed, so `C` can be thought of as
/// * `C = Scattered(Scattered(?, ?), v6)`.
///
/// Now, the node of `v0` also requires remapping, so the result will be:
/// * `Scattered(Scattered(new_var, new_var), new_var)`.
///
/// In the recursion, when we encounter `v5`, we change `require_remapping` to `true` and drop `C`
/// from the list of values (keeping only the scattered values).
/// This signals that inside this subtree, all values need to be remapped (because of the children
/// of `v5`, which are marked by `?` above).
fn compute_remapped_variables(
    values: &[&Value],
    require_remapping: bool,
    parent_path: &MemberPath,
    remapped_callback: &mut impl FnMut(&MemberPath) -> VariableId,
) -> Value {
    if !require_remapping {
        // If all values are the same, no remapping is needed.
        let first_var = values[0];
        if values.iter().all(|x| *x == first_var) {
            return first_var.clone();
        }
    }

    // Collect all the `Value::Scattered` values.
    let only_scattered: Vec<&Box<Scattered>> =
        values.iter().filter_map(|value| try_extract_matches!(value, Value::Scattered)).collect();

    if only_scattered.is_empty() {
        let remapped_var = remapped_callback(parent_path);
        return Value::Var(remapped_var);
    }

    // If we encountered a [Value::Var], we need to remap all the [Value::Scattered] values.
    let require_remapping = require_remapping || only_scattered.len() < values.len();

    let concrete_struct_id = only_scattered[0].concrete_struct_id;
    let members = only_scattered[0]
        .members
        .keys()
        .map(|member_id| {
            let member_path = MemberPath::Member {
                parent: parent_path.clone().into(),
                member_id: *member_id,
                concrete_struct_id,
            };
            // Call `compute_remapped_variables` recursively on the scattered values.
            // If there is a [Value::Var], `require_remapping` will be set to `true` to account
            // for it.
            let member_values =
                only_scattered.iter().map(|scattered| &scattered.members[member_id]).collect_vec();

            (
                *member_id,
                compute_remapped_variables(
                    &member_values,
                    require_remapping,
                    &member_path,
                    remapped_callback,
                ),
            )
        })
        .collect();

    Value::Scattered(Box::new(Scattered { concrete_struct_id, members }))
}

/// A trait for deconstructing and constructing structs.
pub trait StructRecomposer {
    fn deconstruct(
        &mut self,
        concrete_struct_id: semantic::ConcreteStructId,
        value: VariableId,
    ) -> OrderedHashMap<MemberId, VariableId>;

    fn deconstruct_by_types(
        &mut self,
        value: VariableId,
        types: impl Iterator<Item = semantic::TypeId>,
    ) -> Vec<VariableId>;

    fn reconstruct(
        &mut self,
        concrete_struct_id: semantic::ConcreteStructId,
        members: Vec<VariableId>,
    ) -> VariableId;
    fn var_ty(&self, var: VariableId) -> semantic::TypeId;
    fn db(&self) -> &dyn LoweringGroup;
}

/// An intermediate value for a member path.
#[derive(Clone, Debug, DebugWithDb, Eq, PartialEq)]
#[debug_db(ExprFormatter<'a>)]
enum Value {
    /// The value of member path is stored in a lowered variable.
    Var(VariableId),
    /// The value of the member path is not stored. If needed, it should be reconstructed from the
    /// member values.
    Scattered(Box<Scattered>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Var(var) => write!(f, "v{}", var.index()),
            Value::Scattered(scattered) => {
                write!(
                    f,
                    "Scattered({})",
                    scattered.members.values().map(|value| value.to_string()).join(", ")
                )
            }
        }
    }
}

/// A value for a non-stored member path. Recursively holds the [Value] for the members.
#[derive(Clone, Debug, DebugWithDb, Eq, PartialEq)]
#[debug_db(ExprFormatter<'a>)]
struct Scattered {
    concrete_struct_id: semantic::ConcreteStructId,
    members: OrderedHashMap<MemberId, Value>,
}
