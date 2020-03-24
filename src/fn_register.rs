//! Module which defines the function registration mechanism.

#![allow(non_snake_case)]

use crate::any::{Any, Dynamic};
use crate::engine::{Engine, FnCallArgs};
use crate::parser::Position;
use crate::result::EvalAltResult;

use crate::stdlib::{any::TypeId, boxed::Box, string::ToString, vec};

/// A trait to register custom functions with the `Engine`.
pub trait RegisterFn<FN, ARGS, RET> {
    /// Register a custom function with the `Engine`.
    ///
    /// # Example
    ///
    /// ```
    /// # fn main() -> Result<(), rhai::EvalAltResult> {
    /// use rhai::{Engine, RegisterFn};
    ///
    /// // Normal function
    /// fn add(x: i64, y: i64) -> i64 {
    ///     x + y
    /// }
    ///
    /// let mut engine = Engine::new();
    ///
    /// // You must use the trait rhai::RegisterFn to get this method.
    /// engine.register_fn("add", add);
    ///
    /// assert_eq!(engine.eval::<i64>("add(40, 2)")?, 42);
    ///
    /// // You can also register a closure.
    /// engine.register_fn("sub", |x: i64, y: i64| x - y );
    ///
    /// assert_eq!(engine.eval::<i64>("sub(44, 2)")?, 42);
    /// # Ok(())
    /// # }
    /// ```
    fn register_fn(&mut self, name: &str, f: FN);
}

/// A trait to register custom functions that return `Dynamic` values with the `Engine`.
pub trait RegisterDynamicFn<FN, ARGS> {
    /// Register a custom function returning `Dynamic` values with the `Engine`.
    ///
    /// # Example
    ///
    /// ```
    /// # fn main() -> Result<(), rhai::EvalAltResult> {
    /// use rhai::{Engine, Dynamic, RegisterDynamicFn};
    ///
    /// // Function that returns a Dynamic value
    /// fn return_the_same_as_dynamic(x: i64) -> Dynamic {
    ///     Box::new(x)
    /// }
    ///
    /// let mut engine = Engine::new();
    ///
    /// // You must use the trait rhai::RegisterDynamicFn to get this method.
    /// engine.register_dynamic_fn("get_any_number", return_the_same_as_dynamic);
    ///
    /// assert_eq!(engine.eval::<i64>("get_any_number(42)")?, 42);
    /// # Ok(())
    /// # }
    /// ```
    fn register_dynamic_fn(&mut self, name: &str, f: FN);
}

/// A trait to register fallible custom functions returning Result<_, EvalAltResult> with the `Engine`.
pub trait RegisterResultFn<FN, ARGS, RET> {
    /// Register a custom fallible function with the `Engine`.
    ///
    /// # Example
    ///
    /// ```
    /// use rhai::{Engine, RegisterResultFn, EvalAltResult};
    ///
    /// // Normal function
    /// fn div(x: i64, y: i64) -> Result<i64, EvalAltResult> {
    ///     if y == 0 {
    ///         Err("division by zero!".into())     // '.into()' automatically converts to 'EvalAltResult::ErrorRuntime'
    ///     } else {
    ///         Ok(x / y)
    ///     }
    /// }
    ///
    /// let mut engine = Engine::new();
    ///
    /// // You must use the trait rhai::RegisterResultFn to get this method.
    /// engine.register_result_fn("div", div);
    ///
    /// engine.eval::<i64>("div(42, 0)")
    ///         .expect_err("expecting division by zero error!");
    /// ```
    fn register_result_fn(&mut self, name: &str, f: FN);
}

pub struct Ref<A>(A);
pub struct Mut<A>(A);

#[inline]
fn identity<T>(data: T) -> T {
    data
}

macro_rules! count_args {
    () => { 0_usize };
    ( $head:ident $($tail:ident)* ) => { 1_usize + count_args!($($tail)*) };
}

macro_rules! def_register {
    () => {
        def_register!(imp);
    };
    (imp $($par:ident => $mark:ty => $param:ty => $clone:expr),*) => {
        impl<
            $($par: Any + Clone,)*
            FN: Fn($($param),*) -> RET + 'static,
            RET: Any
        > RegisterFn<FN, ($($mark,)*), RET> for Engine<'_>
        {
            fn register_fn(&mut self, name: &str, f: FN) {
                let fn_name = name.to_string();

                let fun = move |mut args: FnCallArgs, pos: Position| {
                    // Check for length at the beginning to avoid per-element bound checks.
                    const NUM_ARGS: usize = count_args!($($par)*);

                    if args.len() != NUM_ARGS {
                        return Err(EvalAltResult::ErrorFunctionArgsMismatch(fn_name.clone(), NUM_ARGS, args.len(), pos));
                    }

                    #[allow(unused_variables, unused_mut)]
                    let mut drain = args.drain(..);
                    $(
                    // Downcast every element, return in case of a type mismatch
                    let $par = drain.next().unwrap().downcast_mut::<$par>().unwrap();
                    )*

                    // Call the user-supplied function using ($clone) to
                    // potentially clone the value, otherwise pass the reference.
                    let r = f($(($clone)($par)),*);
                    Ok(Box::new(r) as Dynamic)
                };
                self.register_fn_raw(name, Some(vec![$(TypeId::of::<$par>()),*]), Box::new(fun));
            }
        }

        impl<
            $($par: Any + Clone,)*
            FN: Fn($($param),*) -> Dynamic + 'static,
        > RegisterDynamicFn<FN, ($($mark,)*)> for Engine<'_>
        {
            fn register_dynamic_fn(&mut self, name: &str, f: FN) {
                let fn_name = name.to_string();

                let fun = move |mut args: FnCallArgs, pos: Position| {
                    // Check for length at the beginning to avoid per-element bound checks.
                    const NUM_ARGS: usize = count_args!($($par)*);

                    if args.len() != NUM_ARGS {
                        return Err(EvalAltResult::ErrorFunctionArgsMismatch(fn_name.clone(), NUM_ARGS, args.len(), pos));
                    }

                    #[allow(unused_variables, unused_mut)]
                    let mut drain = args.drain(..);
                    $(
                    // Downcast every element, return in case of a type mismatch
                    let $par = drain.next().unwrap().downcast_mut::<$par>().unwrap();
                    )*

                    // Call the user-supplied function using ($clone) to
                    // potentially clone the value, otherwise pass the reference.
                    Ok(f($(($clone)($par)),*))
                };
                self.register_fn_raw(name, Some(vec![$(TypeId::of::<$par>()),*]), Box::new(fun));
            }
        }

        impl<
            $($par: Any + Clone,)*
            FN: Fn($($param),*) -> Result<RET, EvalAltResult> + 'static,
            RET: Any
        > RegisterResultFn<FN, ($($mark,)*), RET> for Engine<'_>
        {
            fn register_result_fn(&mut self, name: &str, f: FN) {
                let fn_name = name.to_string();

                let fun = move |mut args: FnCallArgs, pos: Position| {
                    // Check for length at the beginning to avoid per-element bound checks.
                    const NUM_ARGS: usize = count_args!($($par)*);

                    if args.len() != NUM_ARGS {
                        return Err(EvalAltResult::ErrorFunctionArgsMismatch(fn_name.clone(), NUM_ARGS, args.len(), pos));
                    }

                    #[allow(unused_variables, unused_mut)]
                    let mut drain = args.drain(..);
                    $(
                    // Downcast every element, return in case of a type mismatch
                    let $par = drain.next().unwrap().downcast_mut::<$par>().unwrap();
                    )*

                    // Call the user-supplied function using ($clone) to
                    // potentially clone the value, otherwise pass the reference.
                    f($(($clone)($par)),*).map(|r| Box::new(r) as Dynamic).map_err(|mut err| {
                        err.set_position(pos);
                        err
                    })
                };
                self.register_fn_raw(name, Some(vec![$(TypeId::of::<$par>()),*]), Box::new(fun));
            }
        }

        //def_register!(imp_pop $($par => $mark => $param),*);
    };
    ($p0:ident $(, $p:ident)*) => {
        def_register!(imp $p0 => $p0      => $p0      => Clone::clone   $(, $p => $p => $p => Clone::clone)*);
        def_register!(imp $p0 => Ref<$p0> => &$p0     => identity       $(, $p => $p => $p => Clone::clone)*);
        def_register!(imp $p0 => Mut<$p0> => &mut $p0 => identity       $(, $p => $p => $p => Clone::clone)*);

        def_register!($($p),*);
    };
//    (imp_pop) => {};
//    (imp_pop $head:ident => $head_mark:ty => $head_param:ty $(,$tail:ident => $tail_mark:ty => $tp:ty)*) => {
//        def_register!(imp $($tail => $tail_mark => $tp),*);
//    };
}

#[rustfmt::skip]
def_register!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T);
