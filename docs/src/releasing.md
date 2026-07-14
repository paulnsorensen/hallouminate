# Releasing crates

The workspace publishes in dependency order: `hallouminate-domain`, then
`hallouminate-adapters`, then `hallouminate`.

## One-time bootstrap for new crates

crates.io cannot configure a trusted publisher until a crate has its first
release. Before the first workspace tag that includes the domain and adapters
crates:

1. Create a crates.io token allowed to publish new crates.
2. Add it temporarily as the repository secret `CRATES_IO_TOKEN`.
3. Run the **publish-crates** workflow manually with `bootstrap=true`.
   The workflow publishes `hallouminate-domain` first, waits for the sparse
   index, then publishes `hallouminate-adapters`.
4. In crates.io settings for **each** of `hallouminate-domain`,
   `hallouminate-adapters`, and `hallouminate`, add this trusted publisher:
   repository `paulnsorensen/hallouminate`, workflow
   `publish-crates.yml`.
5. Delete the `CRATES_IO_TOKEN` repository secret.

After bootstrap, push the version tag normally. Tagged releases authenticate
only through crates.io Trusted Publishing, check each crate's exact version on
the sparse index, and publish only missing crates in dependency order. This
makes a tag safe when the bootstrap already published the domain and adapters
at that workspace version.
