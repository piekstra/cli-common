# OAuth fixtures

Invented answers in the shapes RFC 6749 §5.1 and §5.2, RFC 7591 §3.2.1 and
SMART App Launch 2.x give, written for these tests rather than captured.
Every token is a `dummy-` string and every id an `example-` or `dummy-` value.
The code exchange's `id_token` is built by the test (`with_id_token`), an
unsigned JWT holding only an example `fhirUser`, so no JWT-shaped string is
committed for a secret scanner to flag. `tests/oauth.rs` checks that
each identity-bearing field keeps that shape.

| File | Shape |
|---|---|
| `token-code.json` | a code exchange's answer with SMART's `patient` and `need_patient_banner`; the test adds its `id_token` |
| `token-refresh.json` | a renewal that sends no refresh token and `expires_in` as a string |
| `token-error-invalid-grant.json` | a spent refresh token |
| `registration.json` | a dynamic registration's answer for a public native client |
