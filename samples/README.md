# Build samples

A flake of small derivations for exercising `flaky build` end to end.
Their outputs do not exist in any binary cache, so the root is always a
real VM build; delete the matching `.cache/erofs/<hash>.erofs` to build
again.

| attr | exercises |
|---|---|
| `write-file` | single-output stdenv (`runCommand`) build |
| `two-outputs` | multi-output build (two output block devices) |
| `raw-derivation` | non-stdenv raw `derivation`, no structured attrs, builder creates `$out` itself via coreutils |
| `file-output` | output is a plain file at `$out` (`echo ... > $out`) |

Outputs are not pre-created in the guest (matching real nix), so all the
usual builder patterns work: `mkdir $out`, `mkdir -p $out`, and writing
a file directly to `$out`.

The `.drv` files are `nix derivation show -r` JSON, regenerated from
`samples/` as its own flake (same pinned nixpkgs as the root flake):

```
cd samples
nix derivation show -r .#packages.aarch64-linux.<name> > <name>.drv
```

Then from the repo root:

```
script -q /dev/null ./target/release/flaky build samples/<name>.drv
```

(`script` allocates a tty, which the VM console requires.)

Note: files in this directory must be git-tracked (`git add`) for the
flake to see them, since the repo is one git tree.
