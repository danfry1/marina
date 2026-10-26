#!/bin/sh
# Builds the site into site/dist. Vercel runs this as the build; run it
# locally to preview dist/.
set -e
cd "$(dirname "$0")"
rm -rf dist && mkdir dist
cp index.html site.js demo.js mark.svg favicon.svg favicon.ico apple-touch-icon.png og.png dist/
