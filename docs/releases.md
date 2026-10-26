# Releases

Chime uses one Release Please version for the virtual Cargo workspace. A release updates the root workspace version, `Cargo.lock`, `release-version.txt`, the Release Please manifest, and `CHANGELOG.md`. Member crates continue to inherit the workspace version. Conventional Commits determine the next version. The seeded baseline is `0.1.0`; the first feature after the bootstrap boundary is expected to produce `v0.2.0`.

## One-time repository setup

Before merging the first Release Please pull request:

1. In **Settings → Actions → General**, allow GitHub Actions to create pull requests. Set workflow permissions to read and write. The workflow narrows permissions by job.
2. In **Settings → Releases**, enable **Immutable releases** and verify that GitHub reports it enabled. The supported API equivalent is `PUT /repos/dstoc/chime/immutable-releases` and requires repository Administration write permission. The workflow does not receive an administrative token and cannot enable or verify the setting.
3. After the first image is pushed, open the `dstoc/chime` package settings and change its visibility to **public**. The workflow performs an unauthenticated pull before it publishes the GitHub release. If the image is still private, the release remains a draft; make the package public and rerun recovery.

No PAT or GitHub App token is needed. The release workflow uses `GITHUB_TOKEN`. Release Please pull-request checks may need a maintainer to approve the run manually under the repository's Actions approval policy.

## Review and operate a release

Release Please runs on pushes to `main`. It opens or updates one release pull request. The workflow checks out that open PR's exact head and runs `cargo update --workspace`. It commits `Cargo.lock` only when the three workspace package versions need synchronization. An unexpected lockfile dependency change or a concurrent push stops the workflow. Review the final PR head and confirm that the workspace manifest, lockfile, Release Please manifest, and `release-version.txt` all use the same version. Approve its required checks if GitHub requires manual approval, then merge the PR to create the draft release and tag.

The same workflow run checks out the tag, builds the Linux AMD64 image and Linux x86-64 GNU CLI, attaches the CLI archive and checksum, records the image digest, creates and verifies Artifact Attestations, tests the CLI in a clean Debian runtime, and anonymously pulls and starts the image. It publishes the GitHub release only after these checks pass. It does not rely on a second workflow triggered by the token-created release.

If a workflow fails, the GitHub release remains a draft. A GHCR image can already be public if a later step failed. After merging a workflow fix, use **Run workflow** with **`main`** selected as the workflow ref and the existing `v<version>` tag entered as input (for example `v0.2.0`). This executes the updated workflow while checking out the original release tag as the immutable build source. It does not move the tag or create a second release. Recovery accepts only the matching draft. It reuses an existing GHCR tag only if its provenance verifies for this repository, release workflow, and tagged source. It reuses a release asset only if its bytes match the deterministic local artifact. It fails without overwriting a mismatched image or asset. A published release is terminal and cannot be used for recovery.

## Download and verify a CLI

Choose a released version. The release includes `chime-v<version>-x86_64-unknown-linux-gnu.tar.gz` and a `.sha256` file. Verify the checksum from the directory containing both files:

```sh
VERSION=0.2.0
ARCHIVE="chime-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
gh release download "v${VERSION}" --repo dstoc/chime --pattern "$ARCHIVE" --pattern "$ARCHIVE.sha256"
sha256sum --check "$ARCHIVE.sha256"
```

The release notes record the source commit. Verify the archive against that commit and this repository's release workflow:

```sh
SOURCE_SHA=<40-character-source-commit-from-release-notes>
gh attestation verify "$ARCHIVE" \
  --repo dstoc/chime \
  --signer-workflow dstoc/chime/.github/workflows/release.yml \
  --source-digest "$SOURCE_SHA"
```

The archive contains the `chime` executable and `LICENSE`. Confirm its embedded version with `./chime --version`. The release build targets x86-64 Linux GNU. It needs the runtime libraries used by the Debian 12 image: glibc, libgcc, OpenSSL 3, zlib, and zstd. Use a compatible Linux distribution or install the matching runtime packages.

## Pull and verify the container image

The release notes and `chime-v<version>-image-digest.txt` asset contain the tag, source commit, and SHA-256 OCI digest. A version tag is convenient for selecting a release. Pin by digest when the deployment must keep the exact image contents:

```sh
VERSION=0.2.0
IMAGE_DIGEST=sha256:<digest-from-release-notes>
podman pull "ghcr.io/dstoc/chime@${IMAGE_DIGEST}"
```

Verify the GitHub Artifact Attestation for the image digest. This command fetches the bundle from GHCR and checks the expected repository, workflow, and source commit:

```sh
SOURCE_SHA=<40-character-source-commit-from-release-notes>
gh attestation verify "oci://ghcr.io/dstoc/chime@${IMAGE_DIGEST}" \
  --repo dstoc/chime \
  --signer-workflow dstoc/chime/.github/workflows/release.yml \
  --source-digest "$SOURCE_SHA" \
  --bundle-from-oci
```

The release workflow refuses to overwrite a version tag it finds in GHCR. It stops if a pre-existing image does not have valid provenance for the expected tagged source. GHCR tags remain mutable outside this workflow; use the recorded digest for deployment pinning and restrict package write access.
