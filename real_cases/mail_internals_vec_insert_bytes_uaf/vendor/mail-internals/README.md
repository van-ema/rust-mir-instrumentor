
# mail-internal

**Provides some internal functionality for the `mail` crate.**

---

The main part of this crate is the `EncodingBuffer` which
is the place the headers write there content to (in an encoded
form). Normally nothing in this crate needs to be used, the
only exception is if you want to write your own mail header
components for your custom mail header. In which case some
of the thinks in this crate might prove usefull for you.
(E.g. the `bind` module which binds some external crates
like e.g. `quoted-string` and `idna`)

Documentation can be [viewed on docs.rs](https://docs.rs/mail-internals)
(once it is published).

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
