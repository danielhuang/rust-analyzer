//! Trait solving using Chalk.

use std::fmt::Debug;
use std::{env::var, sync::Arc};

use chalk_ir::{Fallible, GoalData};
use chalk_recursive::{Cache, UCanonicalGoal};
use chalk_solve::{logging_db::LoggingRustIrDatabase, Solver};

use base_db::CrateId;
use hir_def::{lang_item::LangItemTarget, TraitId};
use stdx::panic_context;
use syntax::SmolStr;

use crate::{
    db::HirDatabase, AliasEq, AliasTy, Canonical, DomainGoal, Goal, Guidance, InEnvironment,
    Interner, Solution, TraitRefExt, Ty, TyKind, WhereClause,
};

#[derive(Debug, Copy, Clone)]
pub(crate) struct ChalkContext<'a> {
    pub(crate) db: &'a dyn HirDatabase,
    pub(crate) krate: CrateId,
}

#[derive(Clone)]
pub struct ChalkCache {
    cache: Arc<Cache<UCanonicalGoal<Interner>, Fallible<Solution>>>,
}

impl PartialEq for ChalkCache {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cache, &other.cache)
    }
}

impl Eq for ChalkCache {}

impl Debug for ChalkCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChalkCache").finish()
    }
}

pub fn chalk_cache(_: &dyn HirDatabase) -> ChalkCache {
    ChalkCache { cache: Arc::new(Cache::new()) }
}

fn create_chalk_solver(db: &dyn HirDatabase) -> chalk_recursive::RecursiveSolver<Interner> {
    let overflow_depth =
        var("CHALK_OVERFLOW_DEPTH").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let max_size = var("CHALK_SOLVER_MAX_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(150);
    chalk_recursive::RecursiveSolver::new(
        overflow_depth,
        max_size,
        Some(Cache::clone(&db.chalk_cache().cache)),
    )
}

/// A set of clauses that we assume to be true. E.g. if we are inside this function:
/// ```rust
/// fn foo<T: Default>(t: T) {}
/// ```
/// we assume that `T: Default`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraitEnvironment {
    pub krate: CrateId,
    // FIXME make this a BTreeMap
    pub(crate) traits_from_clauses: Vec<(Ty, TraitId)>,
    pub env: chalk_ir::Environment<Interner>,
}

impl TraitEnvironment {
    pub fn empty(krate: CrateId) -> Self {
        TraitEnvironment {
            krate,
            traits_from_clauses: Vec::new(),
            env: chalk_ir::Environment::new(Interner),
        }
    }

    pub fn traits_in_scope_from_clauses<'a>(
        &'a self,
        ty: Ty,
    ) -> impl Iterator<Item = TraitId> + 'a {
        self.traits_from_clauses
            .iter()
            .filter_map(move |(self_ty, trait_id)| (*self_ty == ty).then(|| *trait_id))
    }
}

/// Solve a trait goal using Chalk.
// #[cached(convert = r#"{ format!("{:?}", (&krate, &goal)) }"#, key = "String")]
pub(crate) fn trait_solve_query(
    db: &dyn HirDatabase,
    krate: CrateId,
    goal: Canonical<InEnvironment<Goal>>,
) -> Option<Solution> {
    // dbg!(&krate, &goal);
    // dbg!(&*db as *const dyn HirDatabase);

    let _p = profile::span("trait_solve_query").detail(|| match &goal.value.goal.data(Interner) {
        GoalData::DomainGoal(DomainGoal::Holds(WhereClause::Implemented(it))) => {
            db.trait_data(it.hir_trait_id()).name.to_string()
        }
        GoalData::DomainGoal(DomainGoal::Holds(WhereClause::AliasEq(_))) => "alias_eq".to_string(),
        _ => "??".to_string(),
    });
    tracing::info!("trait_solve_query({:?})", goal.value.goal);

    if let GoalData::DomainGoal(DomainGoal::Holds(WhereClause::AliasEq(AliasEq {
        alias: AliasTy::Projection(projection_ty),
        ..
    }))) = &goal.value.goal.data(Interner)
    {
        if let TyKind::BoundVar(_) = projection_ty.self_type_parameter(Interner).kind(Interner) {
            // Hack: don't ask Chalk to normalize with an unknown self type, it'll say that's impossible
            return Some(Solution::Ambig(Guidance::Unknown));
        }
    }

    // We currently don't deal with universes (I think / hope they're not yet
    // relevant for our use cases?)
    let u_canonical = chalk_ir::UCanonical { canonical: goal, universes: 1 };
    solve(db, krate, &u_canonical)
}

fn solve(
    db: &dyn HirDatabase,
    krate: CrateId,
    goal: &chalk_ir::UCanonical<chalk_ir::InEnvironment<chalk_ir::Goal<Interner>>>,
) -> Option<chalk_solve::Solution<Interner>> {
    let _p = profile::span("solve");
    let context = ChalkContext { db, krate };
    tracing::debug!("solve goal: {:?}", goal);
    let mut solver = create_chalk_solver(db);

    let fuel =
        std::cell::Cell::new(var("RA_CHALK_FUEL").ok().and_then(|x| x.parse().ok()).unwrap_or(100));

    let should_continue = || {
        db.unwind_if_cancelled();
        let remaining = fuel.get();
        fuel.set(remaining - 1);
        if remaining == 0 {
            dbg!(&remaining, &krate, &goal);
            tracing::debug!("fuel exhausted");
        }
        remaining > 0
    };

    let mut solve = || {
        let _ctx = if is_chalk_debug() || is_chalk_print() {
            Some(panic_context::enter(format!("solving {:?}", goal)))
        } else {
            None
        };
        let solution = if is_chalk_print() {
            let logging_db =
                LoggingRustIrDatabaseLoggingOnDrop(LoggingRustIrDatabase::new(context));
            solver.solve_limited(&logging_db.0, goal, &should_continue)
        } else {
            solver.solve_limited(&context, goal, &should_continue)
        };

        tracing::debug!("solve({:?}) => {:?}", goal, solution);

        solution
    };

    // don't set the TLS for Chalk unless Chalk debugging is active, to make
    // extra sure we only use it for debugging
    if is_chalk_debug() {
        crate::tls::set_current_program(db, solve)
    } else {
        let _p = profile::span("solve (chalk)");
        solve()
    }
}

struct LoggingRustIrDatabaseLoggingOnDrop<'a>(LoggingRustIrDatabase<Interner, ChalkContext<'a>>);

impl<'a> Drop for LoggingRustIrDatabaseLoggingOnDrop<'a> {
    fn drop(&mut self) {
        eprintln!("chalk program:\n{}", self.0);
    }
}

fn is_chalk_debug() -> bool {
    std::env::var("CHALK_DEBUG").is_ok()
}

fn is_chalk_print() -> bool {
    std::env::var("CHALK_PRINT").is_ok()
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum FnTrait {
    FnOnce,
    FnMut,
    Fn,
}

impl FnTrait {
    const fn lang_item_name(self) -> &'static str {
        match self {
            FnTrait::FnOnce => "fn_once",
            FnTrait::FnMut => "fn_mut",
            FnTrait::Fn => "fn",
        }
    }

    pub fn get_id(&self, db: &dyn HirDatabase, krate: CrateId) -> Option<TraitId> {
        let target = db.lang_item(krate, SmolStr::new_inline(self.lang_item_name()))?;
        match target {
            LangItemTarget::TraitId(t) => Some(t),
            _ => None,
        }
    }
}
