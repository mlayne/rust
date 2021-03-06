// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! See the Book for more information.

pub use self::LateBoundRegionConversionTime::*;
pub use self::RegionVariableOrigin::*;
pub use self::SubregionOrigin::*;
pub use self::ValuePairs::*;
pub use ty::IntVarValue;
pub use self::freshen::TypeFreshener;
pub use self::region_inference::{GenericKind, VerifyBound};

use hir::def_id::DefId;
use hir;
use middle::free_region::FreeRegionMap;
use middle::mem_categorization as mc;
use middle::mem_categorization::McResult;
use middle::region::CodeExtent;
use mir::tcx::LvalueTy;
use ty::subst::{Subst, Substs};
use ty::adjustment;
use ty::{TyVid, IntVid, FloatVid};
use ty::{self, Ty, TyCtxt};
use ty::error::{ExpectedFound, TypeError, UnconstrainedNumeric};
use ty::fold::{TypeFoldable, TypeFolder, TypeVisitor};
use ty::relate::{Relate, RelateResult, TypeRelation};
use traits::{self, PredicateObligations, Reveal};
use rustc_data_structures::unify::{self, UnificationTable};
use std::cell::{Cell, RefCell, Ref, RefMut};
use std::fmt;
use syntax::ast;
use errors::DiagnosticBuilder;
use syntax_pos::{self, Span, DUMMY_SP};
use util::nodemap::{FnvHashMap, FnvHashSet, NodeMap};

use self::combine::CombineFields;
use self::higher_ranked::HrMatchResult;
use self::region_inference::{RegionVarBindings, RegionSnapshot};
use self::unify_key::ToType;

mod bivariate;
mod combine;
mod equate;
pub mod error_reporting;
mod glb;
mod higher_ranked;
pub mod lattice;
mod lub;
pub mod region_inference;
pub mod resolve;
mod freshen;
mod sub;
pub mod type_variable;
pub mod unify_key;

#[must_use]
pub struct InferOk<'tcx, T> {
    pub value: T,
    pub obligations: PredicateObligations<'tcx>,
}
pub type InferResult<'tcx, T> = Result<InferOk<'tcx, T>, TypeError<'tcx>>;

pub type Bound<T> = Option<T>;
pub type UnitResult<'tcx> = RelateResult<'tcx, ()>; // "unify result"
pub type FixupResult<T> = Result<T, FixupError>; // "fixup result"

/// A version of &ty::Tables which can be global or local.
/// Only the local version supports borrow_mut.
#[derive(Copy, Clone)]
pub enum InferTables<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    Global(&'a RefCell<ty::Tables<'gcx>>),
    Local(&'a RefCell<ty::Tables<'tcx>>)
}

impl<'a, 'gcx, 'tcx> InferTables<'a, 'gcx, 'tcx> {
    pub fn borrow(self) -> Ref<'a, ty::Tables<'tcx>> {
        match self {
            InferTables::Global(tables) => tables.borrow(),
            InferTables::Local(tables) => tables.borrow()
        }
    }

    pub fn borrow_mut(self) -> RefMut<'a, ty::Tables<'tcx>> {
        match self {
            InferTables::Global(_) => {
                bug!("InferTables: infcx.tables.borrow_mut() outside of type-checking");
            }
            InferTables::Local(tables) => tables.borrow_mut()
        }
    }
}

pub struct InferCtxt<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    pub tcx: TyCtxt<'a, 'gcx, 'tcx>,

    pub tables: InferTables<'a, 'gcx, 'tcx>,

    // Cache for projections. This cache is snapshotted along with the
    // infcx.
    //
    // Public so that `traits::project` can use it.
    pub projection_cache: RefCell<traits::ProjectionCache<'tcx>>,

    // We instantiate UnificationTable with bounds<Ty> because the
    // types that might instantiate a general type variable have an
    // order, represented by its upper and lower bounds.
    type_variables: RefCell<type_variable::TypeVariableTable<'tcx>>,

    // Map from integral variable to the kind of integer it represents
    int_unification_table: RefCell<UnificationTable<ty::IntVid>>,

    // Map from floating variable to the kind of float it represents
    float_unification_table: RefCell<UnificationTable<ty::FloatVid>>,

    // For region variables.
    region_vars: RegionVarBindings<'a, 'gcx, 'tcx>,

    pub parameter_environment: ty::ParameterEnvironment<'gcx>,

    /// Caches the results of trait selection. This cache is used
    /// for things that have to do with the parameters in scope.
    pub selection_cache: traits::SelectionCache<'tcx>,

    /// Caches the results of trait evaluation.
    pub evaluation_cache: traits::EvaluationCache<'tcx>,

    // the set of predicates on which errors have been reported, to
    // avoid reporting the same error twice.
    pub reported_trait_errors: RefCell<FnvHashSet<traits::TraitErrorKey<'tcx>>>,

    // This is a temporary field used for toggling on normalization in the inference context,
    // as we move towards the approach described here:
    // https://internals.rust-lang.org/t/flattening-the-contexts-for-fun-and-profit/2293
    // At a point sometime in the future normalization will be done by the typing context
    // directly.
    normalize: bool,

    // Sadly, the behavior of projection varies a bit depending on the
    // stage of compilation. The specifics are given in the
    // documentation for `Reveal`.
    projection_mode: Reveal,

    // When an error occurs, we want to avoid reporting "derived"
    // errors that are due to this original failure. Normally, we
    // handle this with the `err_count_on_creation` count, which
    // basically just tracks how many errors were reported when we
    // started type-checking a fn and checks to see if any new errors
    // have been reported since then. Not great, but it works.
    //
    // However, when errors originated in other passes -- notably
    // resolve -- this heuristic breaks down. Therefore, we have this
    // auxiliary flag that one can set whenever one creates a
    // type-error that is due to an error in a prior pass.
    //
    // Don't read this flag directly, call `is_tainted_by_errors()`
    // and `set_tainted_by_errors()`.
    tainted_by_errors_flag: Cell<bool>,

    // Track how many errors were reported when this infcx is created.
    // If the number of errors increases, that's also a sign (line
    // `tained_by_errors`) to avoid reporting certain kinds of errors.
    err_count_on_creation: usize,

    // This flag is used for debugging, and is set to true if there are
    // any obligations set during the current snapshot. In that case, the
    // snapshot can't be rolled back.
    pub obligations_in_snapshot: Cell<bool>,
}

/// A map returned by `skolemize_late_bound_regions()` indicating the skolemized
/// region that each late-bound region was replaced with.
pub type SkolemizationMap = FnvHashMap<ty::BoundRegion, ty::Region>;

/// Why did we require that the two types be related?
///
/// See `error_reporting.rs` for more details
#[derive(Clone, Copy, Debug)]
pub enum TypeOrigin {
    // Not yet categorized in a better way
    Misc(Span),

    // Checking that method of impl is compatible with trait
    MethodCompatCheck(Span),

    // Checking that this expression can be assigned where it needs to be
    // FIXME(eddyb) #11161 is the original Expr required?
    ExprAssignable(Span),

    // Relating trait type parameters to those found in impl etc
    RelateOutputImplTypes(Span),

    // Computing common supertype in the arms of a match expression
    MatchExpressionArm(Span, Span, hir::MatchSource),

    // Computing common supertype in an if expression
    IfExpression(Span),

    // Computing common supertype of an if expression with no else counter-part
    IfExpressionWithNoElse(Span),

    // Computing common supertype in a range expression
    RangeExpression(Span),

    // `where a == b`
    EquatePredicate(Span),

    // `main` has wrong type
    MainFunctionType(Span),

    // `start` has wrong type
    StartFunctionType(Span),

    // intrinsic has wrong type
    IntrinsicType(Span),

    // method receiver
    MethodReceiver(Span),
}

impl TypeOrigin {
    fn as_failure_str(&self) -> &'static str {
        match self {
            &TypeOrigin::Misc(_) |
            &TypeOrigin::RelateOutputImplTypes(_) |
            &TypeOrigin::ExprAssignable(_) => "mismatched types",
            &TypeOrigin::MethodCompatCheck(_) => "method not compatible with trait",
            &TypeOrigin::MatchExpressionArm(_, _, source) => match source {
                hir::MatchSource::IfLetDesugar{..} => "`if let` arms have incompatible types",
                _ => "match arms have incompatible types",
            },
            &TypeOrigin::IfExpression(_) => "if and else have incompatible types",
            &TypeOrigin::IfExpressionWithNoElse(_) => "if may be missing an else clause",
            &TypeOrigin::RangeExpression(_) => "start and end of range have incompatible types",
            &TypeOrigin::EquatePredicate(_) => "equality predicate not satisfied",
            &TypeOrigin::MainFunctionType(_) => "main function has wrong type",
            &TypeOrigin::StartFunctionType(_) => "start function has wrong type",
            &TypeOrigin::IntrinsicType(_) => "intrinsic has wrong type",
            &TypeOrigin::MethodReceiver(_) => "mismatched method receiver",
        }
    }

    fn as_requirement_str(&self) -> &'static str {
        match self {
            &TypeOrigin::Misc(_) => "types are compatible",
            &TypeOrigin::MethodCompatCheck(_) => "method type is compatible with trait",
            &TypeOrigin::ExprAssignable(_) => "expression is assignable",
            &TypeOrigin::RelateOutputImplTypes(_) => {
                "trait type parameters matches those specified on the impl"
            }
            &TypeOrigin::MatchExpressionArm(_, _, _) => "match arms have compatible types",
            &TypeOrigin::IfExpression(_) => "if and else have compatible types",
            &TypeOrigin::IfExpressionWithNoElse(_) => "if missing an else returns ()",
            &TypeOrigin::RangeExpression(_) => "start and end of range have compatible types",
            &TypeOrigin::EquatePredicate(_) => "equality where clause is satisfied",
            &TypeOrigin::MainFunctionType(_) => "`main` function has the correct type",
            &TypeOrigin::StartFunctionType(_) => "`start` function has the correct type",
            &TypeOrigin::IntrinsicType(_) => "intrinsic has the correct type",
            &TypeOrigin::MethodReceiver(_) => "method receiver has the correct type",
        }
    }
}

/// See `error_reporting.rs` for more details
#[derive(Clone, Debug)]
pub enum ValuePairs<'tcx> {
    Types(ExpectedFound<Ty<'tcx>>),
    TraitRefs(ExpectedFound<ty::TraitRef<'tcx>>),
    PolyTraitRefs(ExpectedFound<ty::PolyTraitRef<'tcx>>),
}

/// The trace designates the path through inference that we took to
/// encounter an error or subtyping constraint.
///
/// See `error_reporting.rs` for more details.
#[derive(Clone)]
pub struct TypeTrace<'tcx> {
    origin: TypeOrigin,
    values: ValuePairs<'tcx>,
}

/// The origin of a `r1 <= r2` constraint.
///
/// See `error_reporting.rs` for more details
#[derive(Clone, Debug)]
pub enum SubregionOrigin<'tcx> {
    // Arose from a subtyping relation
    Subtype(TypeTrace<'tcx>),

    // Stack-allocated closures cannot outlive innermost loop
    // or function so as to ensure we only require finite stack
    InfStackClosure(Span),

    // Invocation of closure must be within its lifetime
    InvokeClosure(Span),

    // Dereference of reference must be within its lifetime
    DerefPointer(Span),

    // Closure bound must not outlive captured free variables
    FreeVariable(Span, ast::NodeId),

    // Index into slice must be within its lifetime
    IndexSlice(Span),

    // When casting `&'a T` to an `&'b Trait` object,
    // relating `'a` to `'b`
    RelateObjectBound(Span),

    // Some type parameter was instantiated with the given type,
    // and that type must outlive some region.
    RelateParamBound(Span, Ty<'tcx>),

    // The given region parameter was instantiated with a region
    // that must outlive some other region.
    RelateRegionParamBound(Span),

    // A bound placed on type parameters that states that must outlive
    // the moment of their instantiation.
    RelateDefaultParamBound(Span, Ty<'tcx>),

    // Creating a pointer `b` to contents of another reference
    Reborrow(Span),

    // Creating a pointer `b` to contents of an upvar
    ReborrowUpvar(Span, ty::UpvarId),

    // Data with type `Ty<'tcx>` was borrowed
    DataBorrowed(Ty<'tcx>, Span),

    // (&'a &'b T) where a >= b
    ReferenceOutlivesReferent(Ty<'tcx>, Span),

    // Type or region parameters must be in scope.
    ParameterInScope(ParameterOrigin, Span),

    // The type T of an expression E must outlive the lifetime for E.
    ExprTypeIsNotInScope(Ty<'tcx>, Span),

    // A `ref b` whose region does not enclose the decl site
    BindingTypeIsNotValidAtDecl(Span),

    // Regions appearing in a method receiver must outlive method call
    CallRcvr(Span),

    // Regions appearing in a function argument must outlive func call
    CallArg(Span),

    // Region in return type of invoked fn must enclose call
    CallReturn(Span),

    // Operands must be in scope
    Operand(Span),

    // Region resulting from a `&` expr must enclose the `&` expr
    AddrOf(Span),

    // An auto-borrow that does not enclose the expr where it occurs
    AutoBorrow(Span),

    // Region constraint arriving from destructor safety
    SafeDestructor(Span),
}

/// Places that type/region parameters can appear.
#[derive(Clone, Copy, Debug)]
pub enum ParameterOrigin {
    Path, // foo::bar
    MethodCall, // foo.bar() <-- parameters on impl providing bar()
    OverloadedOperator, // a + b when overloaded
    OverloadedDeref, // *a when overloaded
}

/// Times when we replace late-bound regions with variables:
#[derive(Clone, Copy, Debug)]
pub enum LateBoundRegionConversionTime {
    /// when a fn is called
    FnCall,

    /// when two higher-ranked types are compared
    HigherRankedType,

    /// when projecting an associated type
    AssocTypeProjection(ast::Name),
}

/// Reasons to create a region inference variable
///
/// See `error_reporting.rs` for more details
#[derive(Clone, Debug)]
pub enum RegionVariableOrigin {
    // Region variables created for ill-categorized reasons,
    // mostly indicates places in need of refactoring
    MiscVariable(Span),

    // Regions created by a `&P` or `[...]` pattern
    PatternRegion(Span),

    // Regions created by `&` operator
    AddrOfRegion(Span),

    // Regions created as part of an autoref of a method receiver
    Autoref(Span),

    // Regions created as part of an automatic coercion
    Coercion(Span),

    // Region variables created as the values for early-bound regions
    EarlyBoundRegion(Span, ast::Name),

    // Region variables created for bound regions
    // in a function or method that is called
    LateBoundRegion(Span, ty::BoundRegion, LateBoundRegionConversionTime),

    UpvarRegion(ty::UpvarId, Span),

    BoundRegionInCoherence(ast::Name),
}

#[derive(Copy, Clone, Debug)]
pub enum FixupError {
    UnresolvedIntTy(IntVid),
    UnresolvedFloatTy(FloatVid),
    UnresolvedTy(TyVid)
}

impl fmt::Display for FixupError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::FixupError::*;

        match *self {
            UnresolvedIntTy(_) => {
                write!(f, "cannot determine the type of this integer; \
                           add a suffix to specify the type explicitly")
            }
            UnresolvedFloatTy(_) => {
                write!(f, "cannot determine the type of this number; \
                           add a suffix to specify the type explicitly")
            }
            UnresolvedTy(_) => write!(f, "unconstrained type")
        }
    }
}

/// Helper type of a temporary returned by tcx.infer_ctxt(...).
/// Necessary because we can't write the following bound:
/// F: for<'b, 'tcx> where 'gcx: 'tcx FnOnce(InferCtxt<'b, 'gcx, 'tcx>).
pub struct InferCtxtBuilder<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    global_tcx: TyCtxt<'a, 'gcx, 'gcx>,
    arenas: ty::CtxtArenas<'tcx>,
    tables: Option<RefCell<ty::Tables<'tcx>>>,
    param_env: Option<ty::ParameterEnvironment<'gcx>>,
    projection_mode: Reveal,
    normalize: bool
}

impl<'a, 'gcx, 'tcx> TyCtxt<'a, 'gcx, 'gcx> {
    pub fn infer_ctxt(self,
                      tables: Option<ty::Tables<'tcx>>,
                      param_env: Option<ty::ParameterEnvironment<'gcx>>,
                      projection_mode: Reveal)
                      -> InferCtxtBuilder<'a, 'gcx, 'tcx> {
        InferCtxtBuilder {
            global_tcx: self,
            arenas: ty::CtxtArenas::new(),
            tables: tables.map(RefCell::new),
            param_env: param_env,
            projection_mode: projection_mode,
            normalize: false
        }
    }

    pub fn normalizing_infer_ctxt(self, projection_mode: Reveal)
                                  -> InferCtxtBuilder<'a, 'gcx, 'tcx> {
        InferCtxtBuilder {
            global_tcx: self,
            arenas: ty::CtxtArenas::new(),
            tables: None,
            param_env: None,
            projection_mode: projection_mode,
            normalize: false
        }
    }

    /// Fake InferCtxt with the global tcx. Used by pre-MIR borrowck
    /// for MemCategorizationContext/ExprUseVisitor.
    /// If any inference functionality is used, ICEs will occur.
    pub fn borrowck_fake_infer_ctxt(self, param_env: ty::ParameterEnvironment<'gcx>)
                                    -> InferCtxt<'a, 'gcx, 'gcx> {
        InferCtxt {
            tcx: self,
            tables: InferTables::Global(&self.tables),
            type_variables: RefCell::new(type_variable::TypeVariableTable::new()),
            int_unification_table: RefCell::new(UnificationTable::new()),
            float_unification_table: RefCell::new(UnificationTable::new()),
            region_vars: RegionVarBindings::new(self),
            parameter_environment: param_env,
            selection_cache: traits::SelectionCache::new(),
            evaluation_cache: traits::EvaluationCache::new(),
            projection_cache: RefCell::new(traits::ProjectionCache::new()),
            reported_trait_errors: RefCell::new(FnvHashSet()),
            normalize: false,
            projection_mode: Reveal::NotSpecializable,
            tainted_by_errors_flag: Cell::new(false),
            err_count_on_creation: self.sess.err_count(),
            obligations_in_snapshot: Cell::new(false),
        }
    }
}

impl<'a, 'gcx, 'tcx> InferCtxtBuilder<'a, 'gcx, 'tcx> {
    pub fn enter<F, R>(&'tcx mut self, f: F) -> R
        where F: for<'b> FnOnce(InferCtxt<'b, 'gcx, 'tcx>) -> R
    {
        let InferCtxtBuilder {
            global_tcx,
            ref arenas,
            ref tables,
            ref mut param_env,
            projection_mode,
            normalize
        } = *self;
        let tables = if let Some(ref tables) = *tables {
            InferTables::Local(tables)
        } else {
            InferTables::Global(&global_tcx.tables)
        };
        let param_env = param_env.take().unwrap_or_else(|| {
            global_tcx.empty_parameter_environment()
        });
        global_tcx.enter_local(arenas, |tcx| f(InferCtxt {
            tcx: tcx,
            tables: tables,
            projection_cache: RefCell::new(traits::ProjectionCache::new()),
            type_variables: RefCell::new(type_variable::TypeVariableTable::new()),
            int_unification_table: RefCell::new(UnificationTable::new()),
            float_unification_table: RefCell::new(UnificationTable::new()),
            region_vars: RegionVarBindings::new(tcx),
            parameter_environment: param_env,
            selection_cache: traits::SelectionCache::new(),
            evaluation_cache: traits::EvaluationCache::new(),
            reported_trait_errors: RefCell::new(FnvHashSet()),
            normalize: normalize,
            projection_mode: projection_mode,
            tainted_by_errors_flag: Cell::new(false),
            err_count_on_creation: tcx.sess.err_count(),
            obligations_in_snapshot: Cell::new(false),
        }))
    }
}

impl<T> ExpectedFound<T> {
    fn new(a_is_expected: bool, a: T, b: T) -> Self {
        if a_is_expected {
            ExpectedFound {expected: a, found: b}
        } else {
            ExpectedFound {expected: b, found: a}
        }
    }
}

impl<'tcx, T> InferOk<'tcx, T> {
    pub fn unit(self) -> InferOk<'tcx, ()> {
        InferOk { value: (), obligations: self.obligations }
    }
}

#[must_use = "once you start a snapshot, you should always consume it"]
pub struct CombinedSnapshot {
    projection_cache_snapshot: traits::ProjectionCacheSnapshot,
    type_snapshot: type_variable::Snapshot,
    int_snapshot: unify::Snapshot<ty::IntVid>,
    float_snapshot: unify::Snapshot<ty::FloatVid>,
    region_vars_snapshot: RegionSnapshot,
    obligations_in_snapshot: bool,
}

/// Helper trait for shortening the lifetimes inside a
/// value for post-type-checking normalization.
pub trait TransNormalize<'gcx>: TypeFoldable<'gcx> {
    fn trans_normalize<'a, 'tcx>(&self, infcx: &InferCtxt<'a, 'gcx, 'tcx>) -> Self;
}

macro_rules! items { ($($item:item)+) => ($($item)+) }
macro_rules! impl_trans_normalize {
    ($lt_gcx:tt, $($ty:ty),+) => {
        items!($(impl<$lt_gcx> TransNormalize<$lt_gcx> for $ty {
            fn trans_normalize<'a, 'tcx>(&self,
                                         infcx: &InferCtxt<'a, $lt_gcx, 'tcx>)
                                         -> Self {
                infcx.normalize_projections_in(self)
            }
        })+);
    }
}

impl_trans_normalize!('gcx,
    Ty<'gcx>,
    &'gcx Substs<'gcx>,
    ty::FnSig<'gcx>,
    &'gcx ty::BareFnTy<'gcx>,
    ty::ClosureSubsts<'gcx>,
    ty::PolyTraitRef<'gcx>
);

impl<'gcx> TransNormalize<'gcx> for LvalueTy<'gcx> {
    fn trans_normalize<'a, 'tcx>(&self, infcx: &InferCtxt<'a, 'gcx, 'tcx>) -> Self {
        match *self {
            LvalueTy::Ty { ty } => LvalueTy::Ty { ty: ty.trans_normalize(infcx) },
            LvalueTy::Downcast { adt_def, substs, variant_index } => {
                LvalueTy::Downcast {
                    adt_def: adt_def,
                    substs: substs.trans_normalize(infcx),
                    variant_index: variant_index
                }
            }
        }
    }
}

// NOTE: Callable from trans only!
impl<'a, 'tcx> TyCtxt<'a, 'tcx, 'tcx> {
    pub fn normalize_associated_type<T>(self, value: &T) -> T
        where T: TransNormalize<'tcx>
    {
        debug!("normalize_associated_type(t={:?})", value);

        let value = self.erase_regions(value);

        if !value.has_projection_types() {
            return value;
        }

        self.infer_ctxt(None, None, Reveal::All).enter(|infcx| {
            value.trans_normalize(&infcx)
        })
    }

    pub fn normalize_associated_type_in_env<T>(
        self, value: &T, env: &'a ty::ParameterEnvironment<'tcx>
    ) -> T
        where T: TransNormalize<'tcx>
    {
        debug!("normalize_associated_type_in_env(t={:?})", value);

        let value = self.erase_regions(value);

        if !value.has_projection_types() {
            return value;
        }

        self.infer_ctxt(None, Some(env.clone()), Reveal::All).enter(|infcx| {
            value.trans_normalize(&infcx)
       })
    }
}

impl<'a, 'gcx, 'tcx> InferCtxt<'a, 'gcx, 'tcx> {
    fn normalize_projections_in<T>(&self, value: &T) -> T::Lifted
        where T: TypeFoldable<'tcx> + ty::Lift<'gcx>
    {
        let mut selcx = traits::SelectionContext::new(self);
        let cause = traits::ObligationCause::dummy();
        let traits::Normalized { value: result, obligations } =
            traits::normalize(&mut selcx, cause, value);

        debug!("normalize_projections_in: result={:?} obligations={:?}",
                result, obligations);

        let mut fulfill_cx = traits::FulfillmentContext::new();

        for obligation in obligations {
            fulfill_cx.register_predicate_obligation(self, obligation);
        }

        self.drain_fulfillment_cx_or_panic(DUMMY_SP, &mut fulfill_cx, &result)
    }

    pub fn drain_fulfillment_cx_or_panic<T>(&self,
                                            span: Span,
                                            fulfill_cx: &mut traits::FulfillmentContext<'tcx>,
                                            result: &T)
                                            -> T::Lifted
        where T: TypeFoldable<'tcx> + ty::Lift<'gcx>
    {
        debug!("drain_fulfillment_cx_or_panic()");

        let when = "resolving bounds after type-checking";
        let v = match self.drain_fulfillment_cx(fulfill_cx, result) {
            Ok(v) => v,
            Err(errors) => {
                span_bug!(span, "Encountered errors `{:?}` {}", errors, when);
            }
        };

        match self.tcx.lift_to_global(&v) {
            Some(v) => v,
            None => {
                span_bug!(span, "Uninferred types/regions in `{:?}` {}", v, when);
            }
        }
    }

    /// Finishes processes any obligations that remain in the fulfillment
    /// context, and then "freshens" and returns `result`. This is
    /// primarily used during normalization and other cases where
    /// processing the obligations in `fulfill_cx` may cause type
    /// inference variables that appear in `result` to be unified, and
    /// hence we need to process those obligations to get the complete
    /// picture of the type.
    pub fn drain_fulfillment_cx<T>(&self,
                                   fulfill_cx: &mut traits::FulfillmentContext<'tcx>,
                                   result: &T)
                                   -> Result<T,Vec<traits::FulfillmentError<'tcx>>>
        where T : TypeFoldable<'tcx>
    {
        debug!("drain_fulfillment_cx(result={:?})",
               result);

        // In principle, we only need to do this so long as `result`
        // contains unbound type parameters. It could be a slight
        // optimization to stop iterating early.
        fulfill_cx.select_all_or_error(self)?;

        let result = self.resolve_type_vars_if_possible(result);
        Ok(self.tcx.erase_regions(&result))
    }

    pub fn projection_mode(&self) -> Reveal {
        self.projection_mode
    }

    pub fn freshen<T:TypeFoldable<'tcx>>(&self, t: T) -> T {
        t.fold_with(&mut self.freshener())
    }

    pub fn type_var_diverges(&'a self, ty: Ty) -> bool {
        match ty.sty {
            ty::TyInfer(ty::TyVar(vid)) => self.type_variables.borrow().var_diverges(vid),
            _ => false
        }
    }

    pub fn freshener<'b>(&'b self) -> TypeFreshener<'b, 'gcx, 'tcx> {
        freshen::TypeFreshener::new(self)
    }

    pub fn type_is_unconstrained_numeric(&'a self, ty: Ty) -> UnconstrainedNumeric {
        use ty::error::UnconstrainedNumeric::Neither;
        use ty::error::UnconstrainedNumeric::{UnconstrainedInt, UnconstrainedFloat};
        match ty.sty {
            ty::TyInfer(ty::IntVar(vid)) => {
                if self.int_unification_table.borrow_mut().has_value(vid) {
                    Neither
                } else {
                    UnconstrainedInt
                }
            },
            ty::TyInfer(ty::FloatVar(vid)) => {
                if self.float_unification_table.borrow_mut().has_value(vid) {
                    Neither
                } else {
                    UnconstrainedFloat
                }
            },
            _ => Neither,
        }
    }

    /// Returns a type variable's default fallback if any exists. A default
    /// must be attached to the variable when created, if it is created
    /// without a default, this will return None.
    ///
    /// This code does not apply to integral or floating point variables,
    /// only to use declared defaults.
    ///
    /// See `new_ty_var_with_default` to create a type variable with a default.
    /// See `type_variable::Default` for details about what a default entails.
    pub fn default(&self, ty: Ty<'tcx>) -> Option<type_variable::Default<'tcx>> {
        match ty.sty {
            ty::TyInfer(ty::TyVar(vid)) => self.type_variables.borrow().default(vid),
            _ => None
        }
    }

    pub fn unsolved_variables(&self) -> Vec<ty::Ty<'tcx>> {
        let mut variables = Vec::new();

        let unbound_ty_vars = self.type_variables
                                  .borrow_mut()
                                  .unsolved_variables()
                                  .into_iter()
                                  .map(|t| self.tcx.mk_var(t));

        let unbound_int_vars = self.int_unification_table
                                   .borrow_mut()
                                   .unsolved_variables()
                                   .into_iter()
                                   .map(|v| self.tcx.mk_int_var(v));

        let unbound_float_vars = self.float_unification_table
                                     .borrow_mut()
                                     .unsolved_variables()
                                     .into_iter()
                                     .map(|v| self.tcx.mk_float_var(v));

        variables.extend(unbound_ty_vars);
        variables.extend(unbound_int_vars);
        variables.extend(unbound_float_vars);

        return variables;
    }

    fn combine_fields(&'a self, trace: TypeTrace<'tcx>)
                      -> CombineFields<'a, 'gcx, 'tcx> {
        CombineFields {
            infcx: self,
            trace: trace,
            cause: None,
            obligations: PredicateObligations::new(),
        }
    }

    pub fn equate<T>(&'a self, a_is_expected: bool, trace: TypeTrace<'tcx>, a: &T, b: &T)
        -> InferResult<'tcx, T>
        where T: Relate<'tcx>
    {
        let mut fields = self.combine_fields(trace);
        let result = fields.equate(a_is_expected).relate(a, b);
        result.map(move |t| InferOk { value: t, obligations: fields.obligations })
    }

    pub fn sub<T>(&'a self, a_is_expected: bool, trace: TypeTrace<'tcx>, a: &T, b: &T)
        -> InferResult<'tcx, T>
        where T: Relate<'tcx>
    {
        let mut fields = self.combine_fields(trace);
        let result = fields.sub(a_is_expected).relate(a, b);
        result.map(move |t| InferOk { value: t, obligations: fields.obligations })
    }

    pub fn lub<T>(&'a self, a_is_expected: bool, trace: TypeTrace<'tcx>, a: &T, b: &T)
        -> InferResult<'tcx, T>
        where T: Relate<'tcx>
    {
        let mut fields = self.combine_fields(trace);
        let result = fields.lub(a_is_expected).relate(a, b);
        result.map(move |t| InferOk { value: t, obligations: fields.obligations })
    }

    pub fn glb<T>(&'a self, a_is_expected: bool, trace: TypeTrace<'tcx>, a: &T, b: &T)
        -> InferResult<'tcx, T>
        where T: Relate<'tcx>
    {
        let mut fields = self.combine_fields(trace);
        let result = fields.glb(a_is_expected).relate(a, b);
        result.map(move |t| InferOk { value: t, obligations: fields.obligations })
    }

    fn start_snapshot(&self) -> CombinedSnapshot {
        debug!("start_snapshot()");

        let obligations_in_snapshot = self.obligations_in_snapshot.get();
        self.obligations_in_snapshot.set(false);

        CombinedSnapshot {
            projection_cache_snapshot: self.projection_cache.borrow_mut().snapshot(),
            type_snapshot: self.type_variables.borrow_mut().snapshot(),
            int_snapshot: self.int_unification_table.borrow_mut().snapshot(),
            float_snapshot: self.float_unification_table.borrow_mut().snapshot(),
            region_vars_snapshot: self.region_vars.start_snapshot(),
            obligations_in_snapshot: obligations_in_snapshot,
        }
    }

    fn rollback_to(&self, cause: &str, snapshot: CombinedSnapshot) {
        debug!("rollback_to(cause={})", cause);
        let CombinedSnapshot { projection_cache_snapshot,
                               type_snapshot,
                               int_snapshot,
                               float_snapshot,
                               region_vars_snapshot,
                               obligations_in_snapshot } = snapshot;

        assert!(!self.obligations_in_snapshot.get());
        self.obligations_in_snapshot.set(obligations_in_snapshot);

        self.projection_cache
            .borrow_mut()
            .rollback_to(projection_cache_snapshot);
        self.type_variables
            .borrow_mut()
            .rollback_to(type_snapshot);
        self.int_unification_table
            .borrow_mut()
            .rollback_to(int_snapshot);
        self.float_unification_table
            .borrow_mut()
            .rollback_to(float_snapshot);
        self.region_vars
            .rollback_to(region_vars_snapshot);
    }

    fn commit_from(&self, snapshot: CombinedSnapshot) {
        debug!("commit_from()");
        let CombinedSnapshot { projection_cache_snapshot,
                               type_snapshot,
                               int_snapshot,
                               float_snapshot,
                               region_vars_snapshot,
                               obligations_in_snapshot } = snapshot;

        self.obligations_in_snapshot.set(obligations_in_snapshot);

        self.projection_cache
            .borrow_mut()
            .commit(projection_cache_snapshot);
        self.type_variables
            .borrow_mut()
            .commit(type_snapshot);
        self.int_unification_table
            .borrow_mut()
            .commit(int_snapshot);
        self.float_unification_table
            .borrow_mut()
            .commit(float_snapshot);
        self.region_vars
            .commit(region_vars_snapshot);
    }

    /// Execute `f` and commit the bindings
    pub fn commit_unconditionally<R, F>(&self, f: F) -> R where
        F: FnOnce() -> R,
    {
        debug!("commit()");
        let snapshot = self.start_snapshot();
        let r = f();
        self.commit_from(snapshot);
        r
    }

    /// Execute `f` and commit the bindings if closure `f` returns `Ok(_)`
    pub fn commit_if_ok<T, E, F>(&self, f: F) -> Result<T, E> where
        F: FnOnce(&CombinedSnapshot) -> Result<T, E>
    {
        debug!("commit_if_ok()");
        let snapshot = self.start_snapshot();
        let r = f(&snapshot);
        debug!("commit_if_ok() -- r.is_ok() = {}", r.is_ok());
        match r {
            Ok(_) => { self.commit_from(snapshot); }
            Err(_) => { self.rollback_to("commit_if_ok -- error", snapshot); }
        }
        r
    }

    // Execute `f` in a snapshot, and commit the bindings it creates
    pub fn in_snapshot<T, F>(&self, f: F) -> T where
        F: FnOnce(&CombinedSnapshot) -> T
    {
        debug!("in_snapshot()");
        let snapshot = self.start_snapshot();
        let r = f(&snapshot);
        self.commit_from(snapshot);
        r
    }

    /// Execute `f` and commit only the region bindings if successful.
    /// The function f must be very careful not to leak any non-region
    /// variables that get created.
    pub fn commit_regions_if_ok<T, E, F>(&self, f: F) -> Result<T, E> where
        F: FnOnce() -> Result<T, E>
    {
        debug!("commit_regions_if_ok()");
        let CombinedSnapshot { projection_cache_snapshot,
                               type_snapshot,
                               int_snapshot,
                               float_snapshot,
                               region_vars_snapshot,
                               obligations_in_snapshot } = self.start_snapshot();

        let r = self.commit_if_ok(|_| f());

        debug!("commit_regions_if_ok: rolling back everything but regions");

        assert!(!self.obligations_in_snapshot.get());
        self.obligations_in_snapshot.set(obligations_in_snapshot);

        // Roll back any non-region bindings - they should be resolved
        // inside `f`, with, e.g. `resolve_type_vars_if_possible`.
        self.projection_cache
            .borrow_mut()
            .rollback_to(projection_cache_snapshot);
        self.type_variables
            .borrow_mut()
            .rollback_to(type_snapshot);
        self.int_unification_table
            .borrow_mut()
            .rollback_to(int_snapshot);
        self.float_unification_table
            .borrow_mut()
            .rollback_to(float_snapshot);

        // Commit region vars that may escape through resolved types.
        self.region_vars
            .commit(region_vars_snapshot);

        r
    }

    /// Execute `f` then unroll any bindings it creates
    pub fn probe<R, F>(&self, f: F) -> R where
        F: FnOnce(&CombinedSnapshot) -> R,
    {
        debug!("probe()");
        let snapshot = self.start_snapshot();
        let r = f(&snapshot);
        self.rollback_to("probe", snapshot);
        r
    }

    pub fn add_given(&self,
                     sub: ty::FreeRegion,
                     sup: ty::RegionVid)
    {
        self.region_vars.add_given(sub, sup);
    }

    pub fn sub_types(&self,
                     a_is_expected: bool,
                     origin: TypeOrigin,
                     a: Ty<'tcx>,
                     b: Ty<'tcx>)
        -> InferResult<'tcx, ()>
    {
        debug!("sub_types({:?} <: {:?})", a, b);
        self.commit_if_ok(|_| {
            let trace = TypeTrace::types(origin, a_is_expected, a, b);
            self.sub(a_is_expected, trace, &a, &b).map(|ok| ok.unit())
        })
    }

    pub fn can_sub_types(&self,
                         a: Ty<'tcx>,
                         b: Ty<'tcx>)
                         -> UnitResult<'tcx>
    {
        self.probe(|_| {
            let origin = TypeOrigin::Misc(syntax_pos::DUMMY_SP);
            let trace = TypeTrace::types(origin, true, a, b);
            self.sub(true, trace, &a, &b).map(|_| ())
        })
    }

    pub fn eq_types(&self,
                    a_is_expected: bool,
                    origin: TypeOrigin,
                    a: Ty<'tcx>,
                    b: Ty<'tcx>)
        -> InferResult<'tcx, ()>
    {
        self.commit_if_ok(|_| {
            let trace = TypeTrace::types(origin, a_is_expected, a, b);
            self.equate(a_is_expected, trace, &a, &b).map(|ok| ok.unit())
        })
    }

    pub fn eq_trait_refs(&self,
                          a_is_expected: bool,
                          origin: TypeOrigin,
                          a: ty::TraitRef<'tcx>,
                          b: ty::TraitRef<'tcx>)
        -> InferResult<'tcx, ()>
    {
        debug!("eq_trait_refs({:?} = {:?})", a, b);
        self.commit_if_ok(|_| {
            let trace = TypeTrace {
                origin: origin,
                values: TraitRefs(ExpectedFound::new(a_is_expected, a, b))
            };
            self.equate(a_is_expected, trace, &a, &b).map(|ok| ok.unit())
        })
    }

    pub fn eq_impl_headers(&self,
                           a_is_expected: bool,
                           origin: TypeOrigin,
                           a: &ty::ImplHeader<'tcx>,
                           b: &ty::ImplHeader<'tcx>)
                           -> InferResult<'tcx, ()>
    {
        debug!("eq_impl_header({:?} = {:?})", a, b);
        match (a.trait_ref, b.trait_ref) {
            (Some(a_ref), Some(b_ref)) => self.eq_trait_refs(a_is_expected, origin, a_ref, b_ref),
            (None, None) => self.eq_types(a_is_expected, origin, a.self_ty, b.self_ty),
            _ => bug!("mk_eq_impl_headers given mismatched impl kinds"),
        }
    }

    pub fn sub_poly_trait_refs(&self,
                               a_is_expected: bool,
                               origin: TypeOrigin,
                               a: ty::PolyTraitRef<'tcx>,
                               b: ty::PolyTraitRef<'tcx>)
        -> InferResult<'tcx, ()>
    {
        debug!("sub_poly_trait_refs({:?} <: {:?})", a, b);
        self.commit_if_ok(|_| {
            let trace = TypeTrace {
                origin: origin,
                values: PolyTraitRefs(ExpectedFound::new(a_is_expected, a, b))
            };
            self.sub(a_is_expected, trace, &a, &b).map(|ok| ok.unit())
        })
    }

    pub fn sub_regions(&self,
                       origin: SubregionOrigin<'tcx>,
                       a: ty::Region,
                       b: ty::Region) {
        debug!("sub_regions({:?} <: {:?})", a, b);
        self.region_vars.make_subregion(origin, a, b);
    }

    pub fn equality_predicate(&self,
                              span: Span,
                              predicate: &ty::PolyEquatePredicate<'tcx>)
        -> InferResult<'tcx, ()>
    {
        self.commit_if_ok(|snapshot| {
            let (ty::EquatePredicate(a, b), skol_map) =
                self.skolemize_late_bound_regions(predicate, snapshot);
            let origin = TypeOrigin::EquatePredicate(span);
            let eqty_ok = self.eq_types(false, origin, a, b)?;
            self.leak_check(false, span, &skol_map, snapshot)?;
            self.pop_skolemized(skol_map, snapshot);
            Ok(eqty_ok.unit())
        })
    }

    pub fn region_outlives_predicate(&self,
                                     span: Span,
                                     predicate: &ty::PolyRegionOutlivesPredicate)
        -> UnitResult<'tcx>
    {
        self.commit_if_ok(|snapshot| {
            let (ty::OutlivesPredicate(r_a, r_b), skol_map) =
                self.skolemize_late_bound_regions(predicate, snapshot);
            let origin = RelateRegionParamBound(span);
            self.sub_regions(origin, r_b, r_a); // `b : a` ==> `a <= b`
            self.leak_check(false, span, &skol_map, snapshot)?;
            Ok(self.pop_skolemized(skol_map, snapshot))
        })
    }

    pub fn next_ty_var_id(&self, diverging: bool) -> TyVid {
        self.type_variables
            .borrow_mut()
            .new_var(diverging, None)
    }

    pub fn next_ty_var(&self) -> Ty<'tcx> {
        self.tcx.mk_var(self.next_ty_var_id(false))
    }

    pub fn next_diverging_ty_var(&self) -> Ty<'tcx> {
        self.tcx.mk_var(self.next_ty_var_id(true))
    }

    pub fn next_ty_vars(&self, n: usize) -> Vec<Ty<'tcx>> {
        (0..n).map(|_i| self.next_ty_var()).collect()
    }

    pub fn next_int_var_id(&self) -> IntVid {
        self.int_unification_table
            .borrow_mut()
            .new_key(None)
    }

    pub fn next_float_var_id(&self) -> FloatVid {
        self.float_unification_table
            .borrow_mut()
            .new_key(None)
    }

    pub fn next_region_var(&self, origin: RegionVariableOrigin) -> ty::Region {
        ty::ReVar(self.region_vars.new_region_var(origin))
    }

    /// Create a region inference variable for the given
    /// region parameter definition.
    pub fn region_var_for_def(&self,
                              span: Span,
                              def: &ty::RegionParameterDef)
                              -> ty::Region {
        self.next_region_var(EarlyBoundRegion(span, def.name))
    }

    /// Create a type inference variable for the given
    /// type parameter definition. The substitutions are
    /// for actual parameters that may be referred to by
    /// the default of this type parameter, if it exists.
    /// E.g. `struct Foo<A, B, C = (A, B)>(...);` when
    /// used in a path such as `Foo::<T, U>::new()` will
    /// use an inference variable for `C` with `[T, U]`
    /// as the substitutions for the default, `(T, U)`.
    pub fn type_var_for_def(&self,
                            span: Span,
                            def: &ty::TypeParameterDef<'tcx>,
                            substs: &Substs<'tcx>)
                            -> Ty<'tcx> {
        let default = def.default.map(|default| {
            type_variable::Default {
                ty: default.subst_spanned(self.tcx, substs, Some(span)),
                origin_span: span,
                def_id: def.default_def_id
            }
        });


        let ty_var_id = self.type_variables
                            .borrow_mut()
                            .new_var(false, default);

        self.tcx.mk_var(ty_var_id)
    }

    /// Given a set of generics defined on a type or impl, returns a substitution mapping each
    /// type/region parameter to a fresh inference variable.
    pub fn fresh_substs_for_item(&self,
                                 span: Span,
                                 def_id: DefId)
                                 -> &'tcx Substs<'tcx> {
        Substs::for_item(self.tcx, def_id, |def, _| {
            self.region_var_for_def(span, def)
        }, |def, substs| {
            self.type_var_for_def(span, def, substs)
        })
    }

    pub fn fresh_bound_region(&self, debruijn: ty::DebruijnIndex) -> ty::Region {
        self.region_vars.new_bound(debruijn)
    }

    /// Apply `adjustment` to the type of `expr`
    pub fn adjust_expr_ty(&self,
                          expr: &hir::Expr,
                          adjustment: Option<&adjustment::AutoAdjustment<'tcx>>)
                          -> Ty<'tcx>
    {
        let raw_ty = self.expr_ty(expr);
        let raw_ty = self.shallow_resolve(raw_ty);
        let resolve_ty = |ty: Ty<'tcx>| self.resolve_type_vars_if_possible(&ty);
        raw_ty.adjust(self.tcx,
                      expr.span,
                      expr.id,
                      adjustment,
                      |method_call| self.tables
                                        .borrow()
                                        .method_map
                                        .get(&method_call)
                                        .map(|method| resolve_ty(method.ty)))
    }

    /// True if errors have been reported since this infcx was
    /// created.  This is sometimes used as a heuristic to skip
    /// reporting errors that often occur as a result of earlier
    /// errors, but where it's hard to be 100% sure (e.g., unresolved
    /// inference variables, regionck errors).
    pub fn is_tainted_by_errors(&self) -> bool {
        debug!("is_tainted_by_errors(err_count={}, err_count_on_creation={}, \
                tainted_by_errors_flag={})",
               self.tcx.sess.err_count(),
               self.err_count_on_creation,
               self.tainted_by_errors_flag.get());

        if self.tcx.sess.err_count() > self.err_count_on_creation {
            return true; // errors reported since this infcx was made
        }
        self.tainted_by_errors_flag.get()
    }

    /// Set the "tainted by errors" flag to true. We call this when we
    /// observe an error from a prior pass.
    pub fn set_tainted_by_errors(&self) {
        debug!("set_tainted_by_errors()");
        self.tainted_by_errors_flag.set(true)
    }

    pub fn node_type(&self, id: ast::NodeId) -> Ty<'tcx> {
        match self.tables.borrow().node_types.get(&id) {
            Some(&t) => t,
            // FIXME
            None if self.is_tainted_by_errors() =>
                self.tcx.types.err,
            None => {
                bug!("no type for node {}: {} in fcx",
                     id, self.tcx.map.node_to_string(id));
            }
        }
    }

    pub fn expr_ty(&self, ex: &hir::Expr) -> Ty<'tcx> {
        match self.tables.borrow().node_types.get(&ex.id) {
            Some(&t) => t,
            None => {
                bug!("no type for expr in fcx");
            }
        }
    }

    pub fn resolve_regions_and_report_errors(&self,
                                             free_regions: &FreeRegionMap,
                                             subject_node_id: ast::NodeId) {
        let errors = self.region_vars.resolve_regions(free_regions, subject_node_id);
        if !self.is_tainted_by_errors() {
            // As a heuristic, just skip reporting region errors
            // altogether if other errors have been reported while
            // this infcx was in use.  This is totally hokey but
            // otherwise we have a hard time separating legit region
            // errors from silly ones.
            self.report_region_errors(&errors); // see error_reporting.rs
        }
    }

    pub fn ty_to_string(&self, t: Ty<'tcx>) -> String {
        self.resolve_type_vars_if_possible(&t).to_string()
    }

    pub fn tys_to_string(&self, ts: &[Ty<'tcx>]) -> String {
        let tstrs: Vec<String> = ts.iter().map(|t| self.ty_to_string(*t)).collect();
        format!("({})", tstrs.join(", "))
    }

    pub fn trait_ref_to_string(&self, t: &ty::TraitRef<'tcx>) -> String {
        self.resolve_type_vars_if_possible(t).to_string()
    }

    pub fn shallow_resolve(&self, typ: Ty<'tcx>) -> Ty<'tcx> {
        match typ.sty {
            ty::TyInfer(ty::TyVar(v)) => {
                // Not entirely obvious: if `typ` is a type variable,
                // it can be resolved to an int/float variable, which
                // can then be recursively resolved, hence the
                // recursion. Note though that we prevent type
                // variables from unifying to other type variables
                // directly (though they may be embedded
                // structurally), and we prevent cycles in any case,
                // so this recursion should always be of very limited
                // depth.
                self.type_variables.borrow_mut()
                    .probe(v)
                    .map(|t| self.shallow_resolve(t))
                    .unwrap_or(typ)
            }

            ty::TyInfer(ty::IntVar(v)) => {
                self.int_unification_table
                    .borrow_mut()
                    .probe(v)
                    .map(|v| v.to_type(self.tcx))
                    .unwrap_or(typ)
            }

            ty::TyInfer(ty::FloatVar(v)) => {
                self.float_unification_table
                    .borrow_mut()
                    .probe(v)
                    .map(|v| v.to_type(self.tcx))
                    .unwrap_or(typ)
            }

            _ => {
                typ
            }
        }
    }

    pub fn resolve_type_vars_if_possible<T>(&self, value: &T) -> T
        where T: TypeFoldable<'tcx>
    {
        /*!
         * Where possible, replaces type/int/float variables in
         * `value` with their final value. Note that region variables
         * are unaffected. If a type variable has not been unified, it
         * is left as is.  This is an idempotent operation that does
         * not affect inference state in any way and so you can do it
         * at will.
         */

        if !value.needs_infer() {
            return value.clone(); // avoid duplicated subst-folding
        }
        let mut r = resolve::OpportunisticTypeResolver::new(self);
        value.fold_with(&mut r)
    }

    pub fn resolve_type_and_region_vars_if_possible<T>(&self, value: &T) -> T
        where T: TypeFoldable<'tcx>
    {
        let mut r = resolve::OpportunisticTypeAndRegionResolver::new(self);
        value.fold_with(&mut r)
    }

    /// Resolves all type variables in `t` and then, if any were left
    /// unresolved, substitutes an error type. This is used after the
    /// main checking when doing a second pass before writeback. The
    /// justification is that writeback will produce an error for
    /// these unconstrained type variables.
    fn resolve_type_vars_or_error(&self, t: &Ty<'tcx>) -> mc::McResult<Ty<'tcx>> {
        let ty = self.resolve_type_vars_if_possible(t);
        if ty.references_error() || ty.is_ty_var() {
            debug!("resolve_type_vars_or_error: error from {:?}", ty);
            Err(())
        } else {
            Ok(ty)
        }
    }

    pub fn fully_resolve<T:TypeFoldable<'tcx>>(&self, value: &T) -> FixupResult<T> {
        /*!
         * Attempts to resolve all type/region variables in
         * `value`. Region inference must have been run already (e.g.,
         * by calling `resolve_regions_and_report_errors`).  If some
         * variable was never unified, an `Err` results.
         *
         * This method is idempotent, but it not typically not invoked
         * except during the writeback phase.
         */

        resolve::fully_resolve(self, value)
    }

    // [Note-Type-error-reporting]
    // An invariant is that anytime the expected or actual type is TyError (the special
    // error type, meaning that an error occurred when typechecking this expression),
    // this is a derived error. The error cascaded from another error (that was already
    // reported), so it's not useful to display it to the user.
    // The following methods implement this logic.
    // They check if either the actual or expected type is TyError, and don't print the error
    // in this case. The typechecker should only ever report type errors involving mismatched
    // types using one of these methods, and should not call span_err directly for such
    // errors.

    pub fn type_error_message<M>(&self,
                                 sp: Span,
                                 mk_msg: M,
                                 actual_ty: Ty<'tcx>)
        where M: FnOnce(String) -> String,
    {
        self.type_error_struct(sp, mk_msg, actual_ty).emit();
    }

    // FIXME: this results in errors without an error code. Deprecate?
    pub fn type_error_struct<M>(&self,
                                sp: Span,
                                mk_msg: M,
                                actual_ty: Ty<'tcx>)
                                -> DiagnosticBuilder<'tcx>
        where M: FnOnce(String) -> String,
    {
        self.type_error_struct_with_diag(sp, |actual_ty| {
            self.tcx.sess.struct_span_err(sp, &mk_msg(actual_ty))
        }, actual_ty)
    }

    pub fn type_error_struct_with_diag<M>(&self,
                                          sp: Span,
                                          mk_diag: M,
                                          actual_ty: Ty<'tcx>)
                                          -> DiagnosticBuilder<'tcx>
        where M: FnOnce(String) -> DiagnosticBuilder<'tcx>,
    {
        let actual_ty = self.resolve_type_vars_if_possible(&actual_ty);
        debug!("type_error_struct_with_diag({:?}, {:?})", sp, actual_ty);

        // Don't report an error if actual type is TyError.
        if actual_ty.references_error() {
            return self.tcx.sess.diagnostic().struct_dummy();
        }

        mk_diag(self.ty_to_string(actual_ty))
    }

    pub fn report_mismatched_types(&self,
                                   origin: TypeOrigin,
                                   expected: Ty<'tcx>,
                                   actual: Ty<'tcx>,
                                   err: TypeError<'tcx>) {
        let trace = TypeTrace {
            origin: origin,
            values: Types(ExpectedFound {
                expected: expected,
                found: actual
            })
        };
        self.report_and_explain_type_error(trace, &err).emit();
    }

    pub fn report_conflicting_default_types(&self,
                                            span: Span,
                                            expected: type_variable::Default<'tcx>,
                                            actual: type_variable::Default<'tcx>) {
        let trace = TypeTrace {
            origin: TypeOrigin::Misc(span),
            values: Types(ExpectedFound {
                expected: expected.ty,
                found: actual.ty
            })
        };

        self.report_and_explain_type_error(
            trace,
            &TypeError::TyParamDefaultMismatch(ExpectedFound {
                expected: expected,
                found: actual
            }))
            .emit();
    }

    pub fn replace_late_bound_regions_with_fresh_var<T>(
        &self,
        span: Span,
        lbrct: LateBoundRegionConversionTime,
        value: &ty::Binder<T>)
        -> (T, FnvHashMap<ty::BoundRegion,ty::Region>)
        where T : TypeFoldable<'tcx>
    {
        self.tcx.replace_late_bound_regions(
            value,
            |br| self.next_region_var(LateBoundRegion(span, br, lbrct)))
    }

    /// Given a higher-ranked projection predicate like:
    ///
    ///     for<'a> <T as Fn<&'a u32>>::Output = &'a u32
    ///
    /// and a target trait-ref like:
    ///
    ///     <T as Fn<&'x u32>>
    ///
    /// find a substitution `S` for the higher-ranked regions (here,
    /// `['a => 'x]`) such that the predicate matches the trait-ref,
    /// and then return the value (here, `&'a u32`) but with the
    /// substitution applied (hence, `&'x u32`).
    ///
    /// See `higher_ranked_match` in `higher_ranked/mod.rs` for more
    /// details.
    pub fn match_poly_projection_predicate(&self,
                                           origin: TypeOrigin,
                                           match_a: ty::PolyProjectionPredicate<'tcx>,
                                           match_b: ty::TraitRef<'tcx>)
                                           -> InferResult<'tcx, HrMatchResult<Ty<'tcx>>>
    {
        let span = origin.span();
        let match_trait_ref = match_a.skip_binder().projection_ty.trait_ref;
        let trace = TypeTrace {
            origin: origin,
            values: TraitRefs(ExpectedFound::new(true, match_trait_ref, match_b))
        };

        let match_pair = match_a.map_bound(|p| (p.projection_ty.trait_ref, p.ty));
        let mut combine = self.combine_fields(trace);
        let result = combine.higher_ranked_match(span, &match_pair, &match_b, true)?;
        Ok(InferOk { value: result, obligations: combine.obligations })
    }

    /// See `verify_generic_bound` method in `region_inference`
    pub fn verify_generic_bound(&self,
                                origin: SubregionOrigin<'tcx>,
                                kind: GenericKind<'tcx>,
                                a: ty::Region,
                                bound: VerifyBound) {
        debug!("verify_generic_bound({:?}, {:?} <: {:?})",
               kind,
               a,
               bound);

        self.region_vars.verify_generic_bound(origin, kind, a, bound);
    }

    pub fn can_equate<T>(&self, a: &T, b: &T) -> UnitResult<'tcx>
        where T: Relate<'tcx> + fmt::Debug
    {
        debug!("can_equate({:?}, {:?})", a, b);
        self.probe(|_| {
            // Gin up a dummy trace, since this won't be committed
            // anyhow. We should make this typetrace stuff more
            // generic so we don't have to do anything quite this
            // terrible.
            self.equate(true, TypeTrace::dummy(self.tcx), a, b)
        }).map(|_| ())
    }

    pub fn node_ty(&self, id: ast::NodeId) -> McResult<Ty<'tcx>> {
        let ty = self.node_type(id);
        self.resolve_type_vars_or_error(&ty)
    }

    pub fn expr_ty_adjusted(&self, expr: &hir::Expr) -> McResult<Ty<'tcx>> {
        let ty = self.adjust_expr_ty(expr, self.tables.borrow().adjustments.get(&expr.id));
        self.resolve_type_vars_or_error(&ty)
    }

    pub fn type_moves_by_default(&self, ty: Ty<'tcx>, span: Span) -> bool {
        let ty = self.resolve_type_vars_if_possible(&ty);
        if let Some(ty) = self.tcx.lift_to_global(&ty) {
            // Even if the type may have no inference variables, during
            // type-checking closure types are in local tables only.
            let local_closures = match self.tables {
                InferTables::Local(_) => ty.has_closure_types(),
                InferTables::Global(_) => false
            };
            if !local_closures {
                return ty.moves_by_default(self.tcx.global_tcx(), self.param_env(), span);
            }
        }

        // this can get called from typeck (by euv), and moves_by_default
        // rightly refuses to work with inference variables, but
        // moves_by_default has a cache, which we want to use in other
        // cases.
        !traits::type_known_to_meet_builtin_bound(self, ty, ty::BoundCopy, span)
    }

    pub fn node_method_ty(&self, method_call: ty::MethodCall)
                          -> Option<Ty<'tcx>> {
        self.tables
            .borrow()
            .method_map
            .get(&method_call)
            .map(|method| method.ty)
            .map(|ty| self.resolve_type_vars_if_possible(&ty))
    }

    pub fn node_method_id(&self, method_call: ty::MethodCall)
                          -> Option<DefId> {
        self.tables
            .borrow()
            .method_map
            .get(&method_call)
            .map(|method| method.def_id)
    }

    pub fn adjustments(&self) -> Ref<NodeMap<adjustment::AutoAdjustment<'tcx>>> {
        fn project_adjustments<'a, 'tcx>(tables: &'a ty::Tables<'tcx>)
                                        -> &'a NodeMap<adjustment::AutoAdjustment<'tcx>> {
            &tables.adjustments
        }

        Ref::map(self.tables.borrow(), project_adjustments)
    }

    pub fn is_method_call(&self, id: ast::NodeId) -> bool {
        self.tables.borrow().method_map.contains_key(&ty::MethodCall::expr(id))
    }

    pub fn temporary_scope(&self, rvalue_id: ast::NodeId) -> Option<CodeExtent> {
        self.tcx.region_maps.temporary_scope(rvalue_id)
    }

    pub fn upvar_capture(&self, upvar_id: ty::UpvarId) -> Option<ty::UpvarCapture> {
        self.tables.borrow().upvar_capture_map.get(&upvar_id).cloned()
    }

    pub fn param_env(&self) -> &ty::ParameterEnvironment<'gcx> {
        &self.parameter_environment
    }

    pub fn closure_kind(&self,
                        def_id: DefId)
                        -> Option<ty::ClosureKind>
    {
        if def_id.is_local() {
            self.tables.borrow().closure_kinds.get(&def_id).cloned()
        } else {
            // During typeck, ALL closures are local. But afterwards,
            // during trans, we see closure ids from other traits.
            // That may require loading the closure data out of the
            // cstore.
            Some(self.tcx.closure_kind(def_id))
        }
    }

    pub fn closure_type(&self,
                        def_id: DefId,
                        substs: ty::ClosureSubsts<'tcx>)
                        -> ty::ClosureTy<'tcx>
    {
        if let InferTables::Local(tables) = self.tables {
            if let Some(ty) = tables.borrow().closure_tys.get(&def_id) {
                return ty.subst(self.tcx, substs.func_substs);
            }
        }

        let closure_ty = self.tcx.closure_type(def_id, substs);
        if self.normalize {
            let closure_ty = self.tcx.erase_regions(&closure_ty);

            if !closure_ty.has_projection_types() {
                return closure_ty;
            }

            self.normalize_projections_in(&closure_ty)
        } else {
            closure_ty
        }
    }
}

impl<'a, 'gcx, 'tcx> TypeTrace<'tcx> {
    pub fn span(&self) -> Span {
        self.origin.span()
    }

    pub fn types(origin: TypeOrigin,
                 a_is_expected: bool,
                 a: Ty<'tcx>,
                 b: Ty<'tcx>)
                 -> TypeTrace<'tcx> {
        TypeTrace {
            origin: origin,
            values: Types(ExpectedFound::new(a_is_expected, a, b))
        }
    }

    pub fn dummy(tcx: TyCtxt<'a, 'gcx, 'tcx>) -> TypeTrace<'tcx> {
        TypeTrace {
            origin: TypeOrigin::Misc(syntax_pos::DUMMY_SP),
            values: Types(ExpectedFound {
                expected: tcx.types.err,
                found: tcx.types.err,
            })
        }
    }
}

impl<'tcx> fmt::Debug for TypeTrace<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TypeTrace({:?})", self.origin)
    }
}

impl TypeOrigin {
    pub fn span(&self) -> Span {
        match *self {
            TypeOrigin::MethodCompatCheck(span) => span,
            TypeOrigin::ExprAssignable(span) => span,
            TypeOrigin::Misc(span) => span,
            TypeOrigin::RelateOutputImplTypes(span) => span,
            TypeOrigin::MatchExpressionArm(match_span, _, _) => match_span,
            TypeOrigin::IfExpression(span) => span,
            TypeOrigin::IfExpressionWithNoElse(span) => span,
            TypeOrigin::RangeExpression(span) => span,
            TypeOrigin::EquatePredicate(span) => span,
            TypeOrigin::MainFunctionType(span) => span,
            TypeOrigin::StartFunctionType(span) => span,
            TypeOrigin::IntrinsicType(span) => span,
            TypeOrigin::MethodReceiver(span) => span,
        }
    }
}

impl<'tcx> SubregionOrigin<'tcx> {
    pub fn span(&self) -> Span {
        match *self {
            Subtype(ref a) => a.span(),
            InfStackClosure(a) => a,
            InvokeClosure(a) => a,
            DerefPointer(a) => a,
            FreeVariable(a, _) => a,
            IndexSlice(a) => a,
            RelateObjectBound(a) => a,
            RelateParamBound(a, _) => a,
            RelateRegionParamBound(a) => a,
            RelateDefaultParamBound(a, _) => a,
            Reborrow(a) => a,
            ReborrowUpvar(a, _) => a,
            DataBorrowed(_, a) => a,
            ReferenceOutlivesReferent(_, a) => a,
            ParameterInScope(_, a) => a,
            ExprTypeIsNotInScope(_, a) => a,
            BindingTypeIsNotValidAtDecl(a) => a,
            CallRcvr(a) => a,
            CallArg(a) => a,
            CallReturn(a) => a,
            Operand(a) => a,
            AddrOf(a) => a,
            AutoBorrow(a) => a,
            SafeDestructor(a) => a,
        }
    }
}

impl RegionVariableOrigin {
    pub fn span(&self) -> Span {
        match *self {
            MiscVariable(a) => a,
            PatternRegion(a) => a,
            AddrOfRegion(a) => a,
            Autoref(a) => a,
            Coercion(a) => a,
            EarlyBoundRegion(a, _) => a,
            LateBoundRegion(a, _, _) => a,
            BoundRegionInCoherence(_) => syntax_pos::DUMMY_SP,
            UpvarRegion(_, a) => a
        }
    }
}

impl<'tcx> TypeFoldable<'tcx> for TypeOrigin {
    fn super_fold_with<'gcx: 'tcx, F: TypeFolder<'gcx, 'tcx>>(&self, _folder: &mut F) -> Self {
        self.clone()
    }

    fn super_visit_with<V: TypeVisitor<'tcx>>(&self, _visitor: &mut V) -> bool {
        false
    }
}

impl<'tcx> TypeFoldable<'tcx> for ValuePairs<'tcx> {
    fn super_fold_with<'gcx: 'tcx, F: TypeFolder<'gcx, 'tcx>>(&self, folder: &mut F) -> Self {
        match *self {
            ValuePairs::Types(ref ef) => {
                ValuePairs::Types(ef.fold_with(folder))
            }
            ValuePairs::TraitRefs(ref ef) => {
                ValuePairs::TraitRefs(ef.fold_with(folder))
            }
            ValuePairs::PolyTraitRefs(ref ef) => {
                ValuePairs::PolyTraitRefs(ef.fold_with(folder))
            }
        }
    }

    fn super_visit_with<V: TypeVisitor<'tcx>>(&self, visitor: &mut V) -> bool {
        match *self {
            ValuePairs::Types(ref ef) => ef.visit_with(visitor),
            ValuePairs::TraitRefs(ref ef) => ef.visit_with(visitor),
            ValuePairs::PolyTraitRefs(ref ef) => ef.visit_with(visitor),
        }
    }
}

impl<'tcx> TypeFoldable<'tcx> for TypeTrace<'tcx> {
    fn super_fold_with<'gcx: 'tcx, F: TypeFolder<'gcx, 'tcx>>(&self, folder: &mut F) -> Self {
        TypeTrace {
            origin: self.origin.fold_with(folder),
            values: self.values.fold_with(folder)
        }
    }

    fn super_visit_with<V: TypeVisitor<'tcx>>(&self, visitor: &mut V) -> bool {
        self.origin.visit_with(visitor) || self.values.visit_with(visitor)
    }
}
