#!/bin/sh
set -eu
repository_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
fixture_dir=$repository_dir/tests/fixtures/web-component-probe
node_build_dir=$repository_dir/target/web-component-probe
mkdir -p "$node_build_dir"
printf '{"private":true,"dependencies":{"lit":"3.3.1","esbuild":"0.25.12"}}\n' > "$node_build_dir/package.json"
cp "$fixture_dir/lit-tool.js" "$node_build_dir/source.js"
pnpm --dir "$node_build_dir" install --ignore-scripts
pnpm --dir "$node_build_dir" exec esbuild "$node_build_dir/source.js" --bundle --format=iife --target=es2020 --minify --outfile="$fixture_dir/lit-tool.bundle.js"
