//! Preregistered Workbench performance acceptance definitions.
//!
//! Regular CI compiles but deliberately skips these tests. Their presence and
//! a green default test run are not performance evidence. Before signing off a
//! scenario, implement its measurement body against the production component,
//! run `just perf-workbench` on the reference runner, and record the results in
//! the change under review. Do not tune a budget to fit measurements collected
//! afterward; changing a preregistered budget requires a separate experience
//! and engineering review.
//!
//! Shared reference protocol:
//! - Use a dedicated `x86_64` Ubuntu 24.04 runner with 4 vCPU, 8 GiB RAM, Mesa
//!   lavapipe Vulkan, and no concurrent jobs. Run the release suite with
//!   `just perf-workbench`, which uses one test thread. Record the runner
//!   identity, CPU, RAM, kernel, and locked dependency revision. Until this
//!   controlled runner is provisioned, performance sign-off remains incomplete.
//!   Network, model, and Host latency are outside these local-component budgets
//!   and must be reported separately when included in a broader measurement.
//! - Run each measured iteration in a fresh child process with no other tests.
//!   Execute five complete warmups, return to the same empty initial component
//!   state, then execute one measured iteration. Run twenty measured iterations
//!   at the nominal scale and report the median, nearest-rank p95, and maximum.
//!   Also run 0.5x and 2x input scales. Median total local scenario time at 2x
//!   must not exceed 2.5x the nominal-scale time; available parser, layout, and
//!   rebuild work counters must satisfy the same ratio. This growth check is
//!   separate from the absolute nominal-scale latency budgets.
//! - Read Linux `VmRSS` and `VmHWM`; do not poll for a hypothetical peak. `H` is
//!   `VmHWM` after the measured workload and conservatively includes warmups.
//!   `B0` is `VmRSS` after warmups return to the empty initial state. `B1` is
//!   `VmRSS` after the measured workload with the component alive. `Bdrop` is
//!   `VmRSS` after dropping the component and its workload. For multi-part
//!   workloads, `B1[k]` and `Bdrop[k]` are the corresponding samples after part
//!   `k`, using one-based part numbers. Sample only after input streams are
//!   closed, the runtime is parked, requested frames are rendered, and the
//!   runtime is parked again; never use a sleep to define quiescence.
//!   Source-text, live-object, syntax, layout, and GPU resource counts are the
//!   primary boundedness evidence; RSS is a noisy secondary
//!   budget and cannot prove asymptotic growth by itself.
//! - Use the exact generators below without test-only coalescing. Record every
//!   input parameter, timing percentile, `B0`, `B1`, `Bdrop`, `H`, work counter,
//!   and resource count. Screenshots are optional comparison aids, not saved
//!   performance deliverables.

#![cfg(feature = "workbench")]

/// History uses a 200-message visible window. Let `N` be the events per batch
/// (10,000 nominal, 5,000 at 0.5x, and 20,000 at 2x). For each batch `b` in
/// `0..4` and event `i` in `0..N`, use `seq = b * N + i`. Every tenth event is a
/// user message; the others are title records. Its payload is
/// `format!("batch-{b}-event-{i}-") + "x".repeat(256)`. After every tenth
/// durable event, deliver one assistant-stream frame whose text is
/// `format!("batch-{b}-stream-{}-", i / 10) + "s".repeat(256)`.
/// After batch `b`, let `M = (b + 1) * N / 10` be the cumulative message count
/// and load `min(2,000, M - 200)` older messages, using the Host's natural page
/// sizes and recording every actual page size; then evict back to the
/// 200-message window. Do not assume that a page contains 200 records or that
/// non-message records are capped. Run the full sequence at all three scales.
///
/// Acceptance budgets at nominal scale:
/// - p95 local event/page application and visible feedback is at most 100 ms.
/// - Retained offscreen source text is at most 2 MiB and remains recoverable
///   through Host pagination. Live-object and resource counts return to the
///   same settled value after every batch's eviction.
/// - `H - B0` is at most 64 MiB. `B1[4] - B1[2]` is at most 8 MiB in total,
///   never per batch; this RSS tolerance is not a substitute for the stable
///   object and resource counts.
/// - A live append causes no full visible-projection rebuild; a snapshot or
///   page causes at most one. Eviction releases text, syntax, layout, and GPU
///   resources rather than only hiding rows.
///
/// The evidence must name the production projection owner. A test-only deque
/// or other substitute projection is not a valid measurement boundary.
#[test]
#[ignore = "acceptance definition only; implement and measure on the reference runner"]
fn bounded_history_performance_acceptance() {
    todo!("measure the production bounded-history projection before performance sign-off")
}

/// Build exactly 4,096 independent valid UTF-8 chunks of 256 bytes each (1 MiB
/// total). Cycle through these templates, substituting the zero-based chunk
/// index `{i}`, then pad each template with ASCII spaces to 256 bytes:
/// `"paragraph {i}\n\n"`, `"中文段落 {i}\n\n"`, `"**bold {i}** "`,
/// `"`code {i}` "`, `"```rust\nfn f{i}() {{}}\n```\n\n"`, and
/// `"混合 {i} fn f() {{}} "`. Never split a codepoint. In the paused-reading
/// run, scroll 800 px away from tail after chunk 512. In the Composer run,
/// insert one ASCII `x` after chunks `40, 80, ..., 4,000` (100 inputs).
/// Repeat the complete stream four times at nominal scale, dropping each view
/// before the next, and run 0.5x and 2x chunk counts for the shared growth
/// check. The growth check catches cumulative full-source reparsing even when
/// each chunk performs only one reconciliation call.
///
/// Acceptance budgets at nominal scale:
/// - p95 chunk application plus the next production render is at most 100 ms.
/// - p95 Composer key feedback during streaming is at most 100 ms; selection
///   and the paused reading anchor retain their behavioral guarantees.
/// - `H - B0` is at most 128 MiB. `Bdrop[4] - Bdrop[2]` is at most 8 MiB in
///   total, with matching live object and resource counts after each drop.
/// - Each delivered chunk causes at most one production Markdown
///   reconciliation and one resulting render request. The harness must not
///   batch chunks or omit rendering, and must record bytes scanned or parsed
///   nodes when the component exposes them.
///
/// Report parsing, layout, and rendering timings separately where exposed.
/// Their sum, not network or model generation time, is the local latency.
#[test]
#[ignore = "acceptance definition only; implement and measure on the reference runner"]
fn streaming_markdown_performance_acceptance() {
    todo!("measure the production streaming Markdown path before performance sign-off")
}

/// Generate exactly 100,000 Rust lines by cycling through:
/// `fn function_{i}(value: u64) -> u64 {{ value + {i} }}`,
/// `// 中文注释 {i}: nested module content`,
/// `pub struct Type{i} {{ field: [u64; 4] }}`, and
/// `const VALUE_{i}: &str = "value-{i}";`. Keep the final newline and adjust
/// only ASCII padding in the final comment so the file is exactly 8 MiB.
/// Open it directly in the production Editor. If `L` is the current line count,
/// perform 100 edits at zero-based lines `(i * 997) % L`, appending
/// `" // edit-{i}"`, then scroll down 40 lines 100 times and up 40 lines 100
/// times. Repeat with `L = 50_000` and `L = 200_000` for the shared growth
/// check.
///
/// Acceptance budgets at nominal scale:
/// - p95 initial open, highlight, and first visible frame is at most 2 seconds.
/// - p95 local edit plus visible feedback and p95 scroll feedback are each at
///   most 100 ms.
/// - `H - B0` is at most 256 MiB; `Bdrop - B0` is at most 16 MiB. Live Editor,
///   syntax, layout, and GPU resource counts return to their empty baselines.
/// - An edit causes at most one Editor reconciliation and one incremental
///   highlighter update. Do not disable highlighting, folding, or rendering;
///   count full-file rebuilds and scanned bytes separately.
///
/// This is an Editor component boundary, not a product large-file workflow.
/// The current Host accepts at most 512 KiB of text, so an 8 MiB Host load or
/// save would fail rather than merely add I/O latency. This scenario cannot
/// close product large-file acceptance; that requires a separate Host
/// capability change and its own workflow test.
#[test]
#[ignore = "acceptance definition only; implement and measure on the reference runner"]
fn large_rust_file_performance_acceptance() {
    todo!("measure the production Editor large-file path before performance sign-off")
}
