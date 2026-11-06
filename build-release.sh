#!/bin/sh
# Build every platform and lay it out in ./dist (see the Makefile for details).
set -e
cd "$(dirname "$0")"
exec make all
