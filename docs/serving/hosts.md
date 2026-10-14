# Hosting superfluid in another product

A product can run superfluid as its server: start `superfluid serve` with its own engine and models, and keep its own command line in front. baseRT's `basert serve` does this. This page is the contract such a host builds on; the flags, fields, variables and exit codes named here are kept across releases, and a change to them is announced in the changelog.

## Starting it

- Exec the binary by its real path. superfluid finds `superfluid-workerd` and the adapters it ships beside its own executable, and resolves a symlink to get there, so a link or a shim script on `PATH` works too.
- Name the runtime: `--runtime basert` (or `native`) for `.base` bundles. A model of another format is refused.
- Hand over the engine: `--basert-lib <dir or file>` or `BASERT_LIB` names the libbaseRT to load. Without either, superfluid looks beside its worker, in `../lib`, and in its own runtime installs. On CUDA the engine also needs its kernel sidecar, which it finds beside the executable or at `BASERT_KERNEL_LIB`; a host that keeps the sidecar beside its own binaries sets that variable.
- Pass `--offline` so superfluid installs no runtime and pulls no model by itself; the host resolves model ids.
- Give each server its own `--sessions <dir>`: one server per directory, with a lock. Two servers that share the default collide.
- `SUPERFLUID_HOME` is where superfluid keeps its own installs and socket; a host that wants nothing under `~/.superfluid` sets it.

## Checking compatibility before serving

The engine library and superfluid must agree on the engine's C ABI. Since baseRT 0.3.1 the library states an ABI number (`baseRT_abi_version()`, the header's `BASERT_ABI_VERSION`), and superfluid is built against a header with the same macro. The rule superfluid applies when it opens the library:

- Both sides state a number: they must be equal. The version numbers do not matter.
- Either side has none (a library of 0.3.0 or older, or a superfluid built against an older header): the library's major.minor must match the header's, as before.
- In every case the library must export every function superfluid calls and lay out the model configuration struct as superfluid expects.

A host asks superfluid for its verdict rather than working it out, with the same library it will serve with:

```sh
BASERT_LIB=/path/to/engine superfluid runtimes --json
```

The `basert` row answers:

```json
{
  "id": "basert",
  "state": "ready",
  "ready": true,
  "version": "0.3.1 (/path/to/engine/libbaseRT.dylib)",
  "abi": 1,
  "requires": {"basert_version": "0.3.1", "basert_abi": 1}
}
```

`ready` is `true`, or the reason the library cannot be used, in superfluid's own words. `abi` is the number the loaded library states, absent for a library that states none. `requires` is what this superfluid was built for, present whether or not a library was found, so a host can compare numbers itself.

## Choosing a release

Each release carries `compat.json` beside its tarball (and inside it), the same data as `superfluid compat --json`:

```json
{"superfluid": "0.1.3", "runtimes": {"basert": {"basert_version": "0.3.1", "abi": 1}}}
```

A host that installs superfluid picks the newest release whose `runtimes.basert.abi` equals its engine's number, and falls back to the major.minor of `basert_version` for a release that states no number. The verdict above remains the final word once the release is installed.

## Exit codes

`superfluid serve` exits with 4 when the runtime for the model is not installed or cannot run here, which includes an engine library that is missing or of another ABI. A host can tell that from 2, a flag it got wrong, and from 1, a failure after the start. The full list is in the [command-line reference](cli.md#exit-codes).
