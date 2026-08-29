#!/bin/bash
# usage: token-ratio.sh MODEL SAMPLE_FILE
# Measures a sample string against a model via `claude -p /context`:
# reports the sample system-prompt token count (minus a 7-8 token
# wrapper), total/free context, and call time. chars(SAMPLE_FILE) /
# sys tokens gives the chars-per-token ratio. See
# evals/general-0.1.0 and the roadmap-scan design doc appendix.
M=$1; F=$2
S=$(cat "$F")
START=$(date +%s.%N)
OUT=$(claude -p "/context" --model "$M" --system-prompt "$S" 2>&1)
END=$(date +%s.%N)
SYS=$(echo "$OUT" | grep -oP '\| System prompt \| \K[0-9.]+k?' | head -1)
TOT=$(echo "$OUT" | grep -oP '\*\*Tokens:\*\* \K[0-9.]+[km]? / [0-9.]+[km]?' | head -1)
MODEL=$(echo "$OUT" | grep -oP '\*\*Model:\*\* \K\S+' | head -1)
echo "$M $F model=$MODEL sys=$SYS tokens=$TOT time=$(echo "$END $START" | awk '{printf "%.2f", $1-$2}')"
