## Summary

<!-- Describe what this PR changes and why. -->

## Related Issue

<!-- Closes #... -->

## Type of Change

- [ ] Bug fix (non-breaking change fixing an issue)
- [ ] New feature / enhancement
- [ ] Performance or router resilience improvement
- [ ] Security / Privacy hardening
- [ ] Documentation / Community / CI update

## Verification Checklist

- [ ] Ran `cargo clippy --workspace --all-targets -- -D warnings` (0 warnings)
- [ ] Ran `cargo test --workspace` (all tests pass)
- [ ] Ran `npm --prefix packages/droidpath-cli test` (if CLI or installers changed)
- [ ] Ran `npm --prefix apps/droidpath-desktop test && npm run build` (if Desktop HUD changed)
- [ ] Verified `android-daemon/src` parity and rebuilt `android-daemon.jar` if daemon code changed
- [ ] Confirmed no PII (real passwords, card PANs, OTPs, personal accounts) in tests or Seed Graph data
- [ ] Updated documentation (`README.md` / `README_RU.md`) if user-facing behavior changed
