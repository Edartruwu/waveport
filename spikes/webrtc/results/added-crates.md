# Crates the WebRTC transport would add to the product lockfile

How this was made: the product workspace was copied to a scratch directory (16 November 2026, `main` at e550130), a crate `crates/adapters/transport/webrtc` was added to it with the dependencies of `provider/Cargo.toml` declared through `[workspace.dependencies]`, and the workspace was resolved under the repository's own `.cargo/config.toml` (seven-day cooldown, `incompatible-publish-age = "deny"`). The lockfile gained the 65 crates below and changed no existing entry. "C or assembly" counts files ending in `.c`, `.cc`, `.cpp`, `.h`, `.S`, `.s`, `.asm` in the published crate.

| Crate | Version | Licence | Build script | C or assembly | Published | Days old on 16 Nov 2026 |
|---|---|---|---|---|---|---|
| `aead` | 0.6.1 | MIT OR Apache-2.0 | no | none | 2026-06-17 | 151 |
| `aes` | 0.9.4 | MIT OR Apache-2.0 | no | none | 2026-11-02 | 13 |
| `aes-gcm` | 0.11.1 | Apache-2.0 OR MIT | no | none | 2026-08-21 | 86 |
| `arrayvec` | 0.7.8 | MIT OR Apache-2.0 | no | none | 2026-07-02 | 136 |
| `base16ct` | 1.0.0 | Apache-2.0 OR MIT | no | none | 2026-01-03 | 316 |
| `base64ct` | 1.8.3 | Apache-2.0 OR MIT | no | none | 2026-01-12 | 307 |
| `ccm` | 0.6.1 | Apache-2.0 OR MIT | no | none | 2026-08-21 | 86 |
| `chacha20poly1305` | 0.11.0 | Apache-2.0 OR MIT | no | none | 2026-06-28 | 140 |
| `cipher` | 0.5.2 | MIT OR Apache-2.0 | no | none | 2026-05-19 | 180 |
| `cmov` | 0.5.4 | Apache-2.0 OR MIT | no | none | 2026-05-28 | 171 |
| `cpubits` | 0.1.1 | MIT OR Apache-2.0 | no | none | 2026-04-28 | 201 |
| `crc` | 3.4.0 | MIT OR Apache-2.0 | no | none | 2025-11-26 | 354 |
| `crc-catalog` | 2.5.0 | MIT OR Apache-2.0 | no | none | 2026-04-24 | 205 |
| `crypto-bigint` | 0.7.11 | Apache-2.0 OR MIT | no | none | 2026-11-08 | 7 |
| `ctr` | 0.10.1 | MIT OR Apache-2.0 | no | none | 2026-05-20 | 179 |
| `ctutils` | 0.4.8 | Apache-2.0 OR MIT | no | none | 2026-11-02 | 13 |
| `curve25519-dalek` | 5.0.0 | BSD-3-Clause | yes | none | 2026-07-06 | 132 |
| `curve25519-dalek-derive` | 0.1.1 | MIT/Apache-2.0 | no | none | 2023-10-31 | 1111 |
| `der` | 0.8.3 | Apache-2.0 OR MIT | no | none | 2026-10-25 | 21 |
| `der_derive` | 0.8.0 | Apache-2.0 OR MIT | no | none | 2026-02-11 | 277 |
| `deranged` | 0.5.9 | MIT OR Apache-2.0 | no | none | 2026-10-13 | 33 |
| `dimpl` | 0.9.0 | MIT OR Apache-2.0 | no | none | 2026-10-25 | 21 |
| `ecdsa` | 0.17.0 | Apache-2.0 OR MIT | no | none | 2026-07-02 | 136 |
| `elliptic-curve` | 0.14.3 | Apache-2.0 OR MIT | no | none | 2026-10-25 | 21 |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT | no | none | 2026-07-19 | 119 |
| `ff` | 0.14.0 | MIT/Apache-2.0 | no | none | 2026-05-30 | 169 |
| `fiat-crypto` | 0.3.0 | MIT OR Apache-2.0 OR BSD-1-Clause | no | none | 2025-06-04 | 529 |
| `flagset` | 0.4.7 | Apache-2.0 | no | none | 2025-04-04 | 590 |
| `ghash` | 0.6.0 | Apache-2.0 OR MIT | no | none | 2026-02-28 | 260 |
| `group` | 0.14.0 | MIT/Apache-2.0 | no | none | 2026-06-01 | 167 |
| `hkdf` | 0.13.0 | MIT OR Apache-2.0 | no | none | 2026-03-30 | 230 |
| `hmac` | 0.13.0 | MIT OR Apache-2.0 | no | none | 2026-03-29 | 231 |
| `inout` | 0.2.6 | MIT OR Apache-2.0 | no | none | 2026-11-07 | 8 |
| `is` | 0.12.0 | MIT OR Apache-2.0 | no | none | 2026-11-03 | 12 |
| `nom` | 8.0.0 | MIT | no | none | 2025-01-26 | 658 |
| `num-conv` | 0.2.2 | MIT OR Apache-2.0 | no | none | 2026-05-18 | 181 |
| `p256` | 0.14.0 | Apache-2.0 OR MIT | no | none | 2026-07-03 | 135 |
| `p384` | 0.14.0 | Apache-2.0 OR MIT | no | none | 2026-07-06 | 132 |
| `pem-rfc7468` | 1.0.0 | Apache-2.0 OR MIT | no | none | 2025-11-08 | 372 |
| `pkcs8` | 0.11.0 | Apache-2.0 OR MIT | no | none | 2026-04-27 | 202 |
| `poly1305` | 0.9.1 | Apache-2.0 OR MIT | no | none | 2026-07-08 | 130 |
| `polyval` | 0.7.3 | Apache-2.0 OR MIT | no | none | 2026-07-17 | 121 |
| `powerfmt` | 0.2.1 | MIT OR Apache-2.0 | no | none | 2026-10-05 | 41 |
| `primefield` | 0.14.0 | Apache-2.0 OR MIT | no | none | 2026-06-24 | 144 |
| `primeorder` | 0.14.0 | Apache-2.0 OR MIT | no | none | 2026-07-03 | 135 |
| `rfc6979` | 0.6.0 | Apache-2.0 OR MIT | no | none | 2026-06-30 | 138 |
| `rustc-hash` | 2.1.3 | Apache-2.0 OR MIT | no | none | 2026-07-02 | 136 |
| `sctp-proto` | 0.10.8 | MIT OR Apache-2.0 | no | none | 2026-11-01 | 14 |
| `sec1` | 0.8.1 | Apache-2.0 OR MIT | no | none | 2026-03-31 | 229 |
| `serdect` | 0.4.3 | Apache-2.0 OR MIT | no | none | 2026-05-04 | 195 |
| `signature` | 3.0.0 | Apache-2.0 OR MIT | no | none | 2026-05-02 | 197 |
| `spki` | 0.8.1 | Apache-2.0 OR MIT | no | none | 2026-10-05 | 41 |
| `str0m` | 0.25.0 | MIT OR Apache-2.0 | no | none | 2026-11-03 | 12 |
| `str0m-proto` | 0.8.0 | MIT OR Apache-2.0 | no | none | 2026-11-03 | 12 |
| `time` | 0.3.55 | MIT OR Apache-2.0 | no | none | 2026-08-01 | 106 |
| `time-core` | 0.1.9 | MIT OR Apache-2.0 | no | none | 2026-06-12 | 156 |
| `time-macros` | 0.2.32 | MIT OR Apache-2.0 | no | none | 2026-07-20 | 118 |
| `tls_codec` | 0.4.2 | Apache-2.0 OR MIT | no | none | 2025-02-10 | 643 |
| `tls_codec_derive` | 0.4.2 | Apache-2.0 OR MIT | no | none | 2025-02-10 | 643 |
| `tracing-attributes` | 0.1.31 | MIT | no | none | 2025-11-26 | 354 |
| `universal-hash` | 0.6.1 | MIT OR Apache-2.0 | no | none | 2026-02-27 | 261 |
| `wnaf` | 0.14.1 | Apache-2.0 OR MIT | no | none | 2026-09-03 | 73 |
| `x25519-dalek` | 3.0.0 | BSD-3-Clause | no | none | 2026-07-06 | 132 |
| `x509-cert` | 0.3.0 | Apache-2.0 OR MIT | no | none | 2026-07-09 | 129 |
| `zeroize_derive` | 1.5.0 | Apache-2.0 OR MIT | no | none | 2026-06-12 | 156 |
