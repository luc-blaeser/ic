#!/bin/bash

function usage() {
    cat <<EOF
Usage:
  generate-monitoring-canister-adapter-config -o ic-monitoring-canister-adapter.json

  Generate the monitoring canister adapter config.

  -o outfile: output ic-monitoring-canister-adapter.json file
EOF
}

while getopts "o:" OPT; do
    case "${OPT}" in
        o)
            OUT_FILE="${OPTARG}"
            ;;
        *)
            usage
            exit 1
            ;;
    esac
done

if [ "${OUT_FILE}" == "" ]; then
    usage
    exit 1
fi

echo '{
    "logger": {
        "format": "json",
        "level": "info"
    }
}' >$OUT_FILE

# umask for service is set to be restricted, but this file needs to be
# world-readable
chmod 644 "${OUT_FILE}"
