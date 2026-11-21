#!/bin/sh
# Reference pipeline: ./oracle.sh DATA_DIR OUT_DIR   (optional env DUMP_DIR=dir for stage dumps)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PYTHONPATH="${PYTHONPATH:+$PYTHONPATH:}$HOME/pylib" exec python3 "$HERE/oracle/oracle.py" "$@"
