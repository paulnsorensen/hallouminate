# Releasing crates

The workspace publishes in dependency order: `hallouminate-domain`, then
`hallouminate-adapters`, then `hallouminate-config`, then `hallouminate-daemon`,
then `hallouminate`.

## One-time bootstrap for new crates

crates.io cannot configure a trusted publisher until a crate has its first
release. Any crate that has never been published — currently
`hallouminate-config` and `hallouminate-daemon` — must be bootstrapped once
before the first workspace tag that ships it:

1. Create a crates.io token allowed to publish new crates.
2. Add it temporarily as the repository secret `CRATES_IO_TOKEN`.
3. Run the **publish-crates** workflow manually with `bootstrap=true`.
   The workflow walks the library crates in dependency order
   (`hallouminate-domain`, `hallouminate-adapters`, `hallouminate-config`,
   `hallouminate-daemon`), waiting for the sparse index between each, and
   publishes only the ones missing at the current workspace version.
4. In crates.io settings for **each** newly published crate
   (`hallouminate-config`, `hallouminate-daemon`), add this trusted publisher:
   repository `paulnsorensen/hallouminate`, workflow `publish-crates.yml`.
   (`hallouminate-domain`, `hallouminate-adapters`, and `hallouminate` already
   have theirs from the initial bootstrap.)
5. Delete the `CRATES_IO_TOKEN` repository secret.

After bootstrap, push the version tag normally. Tagged releases authenticate
only through crates.io Trusted Publishing, check each crate's exact version on
the sparse index, and publish only missing crates in dependency order. This
makes a tag safe when the bootstrap already published the new crates at that
workspace version.
