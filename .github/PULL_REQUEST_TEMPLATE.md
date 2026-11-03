## Summary

<!-- Explain the problem, what changes, and why. Include a before/after example when useful. -->

## Type of change

<!-- Select all that apply. -->

- [ ] Bug fix
- [ ] Feature or improvement
- [ ] Refactor
- [ ] Documentation
- [ ] Tests, CI, or developer tooling
- [ ] Dependency update

## Related issues

<!-- Link related issues. Use "Closes #123" only if this PR resolves the issue. An issue is optional for small fixes. -->

## Verification

<!-- Run the checks applicable to this PR from the repository root. Check a box only after its command passes. Mark unrelated groups N/A with a short reason; explain failed or unrun checks below. -->
<!-- Additional minimum-Rust, dependency security, and focused-test commands are in CONTRIBUTING.md#checks-before-opening-a-pull-request. -->

### Rust workspace

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace --all-targets --profile ci`
- [ ] `python3 -m unittest discover -s .github/scripts -p 'test_*.py'`

### Desktop frontend

- [ ] `npm --prefix apps/desktop ci`
- [ ] `npm --prefix apps/desktop run build`
- [ ] `npm --prefix apps/desktop test`

### Desktop native shell

<!-- Build the frontend above and prepare the backend before running the native shell checks. -->

- [ ] `npm --prefix apps/desktop run prepare:backend`
- [ ] `cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml -- --check`
- [ ] `cargo clippy --manifest-path apps/desktop/src-tauri/Cargo.toml --all-targets --locked -- -D warnings`
- [ ] `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --locked --no-fail-fast`

### Additional checks when applicable

- [ ] Artifact rendering on macOS: `npm --prefix apps/desktop run test:artifacts:webkit`
- [ ] Scenario definitions: `cargo run -p medha-cli --bin medha -- gate scenarios/ --validate`
- [ ] Dependency changes: the applicable [security checks](https://github.com/kashyaprajharsh/medha-agent/blob/main/CONTRIBUTING.md#dependency-security-checks) pass.

### Results and checks not run

<!-- Record focused tests, additional commands, failures, skips, and N/A groups with their reasons. Documentation-only PRs can mark application checks N/A. -->
<!-- For UI changes, describe the flow tested in a build from this branch and add screenshots or a recording where useful. -->
<!-- For model evaluations, record the model, provider, configuration, and results. Distinguish scenario validation from an actual model run. -->

## Compatibility and risk

<!-- Describe relevant behavior, protocol, persistent-data, permission, credential, or platform implications. Include migration or rollback instructions when needed. Write "None" if there are none. -->

## Checklist

- [ ] I read the [contributing guide](https://github.com/kashyaprajharsh/medha-agent/blob/main/CONTRIBUTING.md) and [Code of Conduct](https://github.com/kashyaprajharsh/medha-agent/blob/main/CODE_OF_CONDUCT.md).
- [ ] For Rust changes, I followed the [Rust contribution guide](https://github.com/kashyaprajharsh/medha-agent/blob/main/RUST_GUIDELINES.md), including the applicable review or porting requirements.
- [ ] I kept this change focused and reviewed the diff for accidental files, secrets, and private data.
- [ ] `git diff --check` passes.
- [ ] I ran the relevant checks and recorded their results above, or explained why they could not run.
- [ ] I added appropriate regression coverage or explained why it is unnecessary.
- [ ] I updated affected documentation, or confirmed that no update is needed.

<!-- Suspected vulnerabilities belong in the private reporting channel described in SECURITY.md, not in a public PR. -->
