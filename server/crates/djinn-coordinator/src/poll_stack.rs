//! Heap detours for the coordinator's deeply nested `async fn` chain.
//!
//! # Why this module exists
//!
//! `CoordinatorActor::run` is the root of an eight-deep chain of nested
//! `async fn`s (`run` → `run_dispatch_loop` → `run_pass_with_watchdog` →
//! `run_tick` → `dispatch_ready_tasks` → …), and every link is polled inside
//! the frame of the link above it. That is fine in a release build, but the
//! debug/test profile is compiled at `opt-level = 0`, where LLVM does **not**
//! run its stack-colouring pass: every `alloca` in a function gets its own
//! distinct stack slot, and none are ever reused.
//!
//! For an `async fn` that is the difference between a small frame and an
//! enormous one. Awaiting a sub-future costs the enclosing coroutine's poll
//! frame **two** slots the size of that sub-future — one to build it in, one to
//! move it into the coroutine state — and, with no reuse, those slots are
//! summed over *every* `await` in the body rather than overlapped. A pass with
//! thirty awaits therefore reserves roughly `2 × Σ size_of(sub-future)` bytes of
//! stack, and the chain multiplies out: measured on `main`, one poll of the 30s
//! tick consumed 2,210,352 bytes — 105% of tokio's 2 MiB worker stack — and the
//! worst-case path through the actor reached 3,666,896 bytes (175%). The
//! `--lib` test suite aborted with `has overflowed its stack`.
//!
//! # Why [`boxed`] takes a closure
//!
//! The obvious fix, `Box::pin(self.dispatch_ready_tasks(None)).await`, only
//! halves the cost. `Box::pin` takes its argument **by value**, so the caller
//! still has to materialise the whole sub-future in one of its own stack slots
//! before handing it over; only the second slot (the copy into the coroutine
//! state) is saved. Measured on a synthetic ten-await chain: 451,336 → 226,296
//! bytes.
//!
//! Passing a *thunk* instead moves the construction into [`boxed`]'s own frame.
//! The caller only ever materialises the closure (a couple of captured
//! references) and receives back a 16-byte fat pointer, so its frame stops
//! scaling with the size of what it awaits entirely — the same synthetic chain
//! drops to 536 bytes. [`boxed`]'s frame does hold the sub-future while it is
//! being constructed, but that frame is popped before the returned future is
//! ever polled, so it never stacks with the poll chain underneath it.
//!
//! # When to use it
//!
//! At the *nesting points*: an `await` on another sizeable coordinator pass.
//! It is not worth applying to small leaf awaits — each call costs one heap
//! allocation, and the frames that matter are the ones that nest.

use std::future::Future;
use std::pin::Pin;

/// Await a coordinator sub-pass on the heap instead of in the caller's poll
/// frame.
///
/// `make` is called immediately; the future it returns is boxed inside *this*
/// function's stack frame, which is gone by the time the caller polls the
/// result. See the [module docs](self) for why this takes a closure rather
/// than the future itself.
///
/// ```ignore
/// // before: costs run_tick's poll frame 2 × size_of(dispatch_ready_tasks future)
/// self.dispatch_ready_tasks(None).await;
/// // after: costs it 16 bytes
/// poll_stack::boxed(|| self.dispatch_ready_tasks(None)).await;
/// ```
pub(crate) fn boxed<'a, T, F>(
    make: impl FnOnce() -> F,
) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
where
    T: 'a,
    F: Future<Output = T> + Send + 'a,
{
    Box::pin(make())
}
