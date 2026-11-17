# Security

Report a vulnerability privately through GitHub's "Report a vulnerability"
on this repository. Please do not open a public issue.

## What athena protects

- **Tokens** live only in athena's vault: each sign-in encrypted
  (XChaCha20-Poly1305) in its own owner-only file under
  `~/.config/athena/vault/`, with the key in the OS keychain (service
  `piekstra.athena`, account `vault-key`). They are never printed, logged,
  written to a file in the clear, or passed in argv. `athena auth logout`
  revokes them at athenahealth and deletes the entry, and a second sign-in
  for the same patient revokes the tokens the one it replaces held alone
  (nothing, when both are one renewed grant, so the new one stays live).
- **Profiles** keep one person's login apart from another's: each has its own
  sign-ins and its own Chrome profile for signing in, and every command that
  uses a sign-in must name its profile.
- **No client secret exists**: athena is a public client, and each sign-in is
  bound to its PKCE verifier (S256), so a stolen authorization code is
  useless.
- **The sign-in redirect** goes to `127.0.0.1` only, and its `state`, the
  issuer it names and the id token's `nonce` are checked.
- **Tokens go only to the practice's own FHIR server**: paths, next links and
  attachment URLs on another origin are refused or skipped, and redirects are
  not followed with a token.
- **Renewals** are serialized across processes by a refresh lock, so two
  processes never present one refresh token at once.
- **Records** go to stdout (`--ndjson`) or, for athenahealth's sandbox and
  local test servers only, to a `0600` file in a `0700` directory. Saved
  documents are `0600`; config and the session index are `0600`.

`athena` is read-only: it makes no request that changes a record.
