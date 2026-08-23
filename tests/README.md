# Test suite layout

Keep syntax acceptance separate from rule evaluation and filesystem delivery so
a failure identifies the layer whose behavior changed.

- Parser tests live beside the parser in `src/config/parser.rs`. They inspect
  the typed configuration or a bounded syntax error and do not process a mail
  message or touch a destination.
- Evaluation tests live beside the evaluator in `src/eval.rs` and in
  `differential_eval.rs`. They use an in-memory delivery recorder and do not
  exercise Maildir or mbox serialization.
- Delivery backend tests live under `src/delivery/`. They validate Maildir and
  mbox filesystem behavior without relying on rc parsing.
- CLI tests in `cli.rs` cover only behavior that crosses these boundaries,
  such as streaming stdin through evaluation into a real destination.
- Long-running delivery stress tests live in `stress_delivery.rs` and are
  ignored by default. Run them explicitly with
  `cargo test --locked --test stress_delivery -- --ignored --test-threads=1`.
- Stored compatibility fixtures retain the reference rc files and reviewed
  results, but ordinary tests execute only `procmail-rs`.

Do not turn a parser case into an end-to-end test merely to reach parser APIs.
When a feature spans layers, add a narrow test to each affected layer and keep
only the final integration scenario in `cli.rs`.
