"""Build real plugin fixtures, then run workspace tests without external sites."""
import argparse
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def cargo(*args):
    subprocess.run(["cargo", *args], cwd=ROOT, check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--wasm-demo-only", action="store_true",
                        help="Build and check the installed production WASM demo parser only")
    args = parser.parse_args()
    if not args.wasm_demo_only:
        cargo("build", "--locked", "-p", "archive-sift-demo-plugin", "--lib",
              "--target", "wasm32-unknown-unknown", "--release", "--features", "test-fixtures")
        cargo("build", "--locked", "-p", "archive-sift-demo-plugin", "--bin",
              "archive-sift-demo-process", "--features", "test-fixtures")
        cargo("run", "--locked", "-p", "archive-sift-plugin-packager", "--", "wasm",
              "target/wasm32-unknown-unknown/release/archive_sift_demo_plugin.wasm",
              "target/test-plugins/wasm", "https://example.test")
        executable = "target/debug/archive-sift-demo-process" + (".exe" if os.name == "nt" else "")
        cargo("run", "--locked", "-p", "archive-sift-plugin-packager", "--", "process",
              executable, "target/test-plugins/process", "https://example.test")
    # Package the production component separately, without hostile test exports.
    # Copy it before another build can replace the core module in target/.
    cargo("build", "--locked", "-p", "archive-sift-demo-plugin", "--lib",
          "--target", "wasm32-unknown-unknown", "--release", "--no-default-features")
    cargo("run", "--locked", "-p", "archive-sift-plugin-packager", "--", "wasm",
          "target/wasm32-unknown-unknown/release/archive_sift_demo_plugin.wasm",
          "target/test-plugins/demo-wasm", "https://example.test")
    if not args.prepare_only:
        if args.wasm_demo_only:
            cargo("test", "--locked", "--test", "wasm_demo_site")
        else:
            cargo("test", "--locked", "--workspace")


if __name__ == "__main__":
    main()
