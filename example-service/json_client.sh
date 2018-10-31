#!/usr/bin/env sh
join(){
    local str=
    local i
    for i; do
        str="$str, $i"
    done
    echo ${str#, }
}

parseResponse(){
    python -c "
import json
import sys
resp = json.load(sys.stdin)['message']
if resp['Ok']:
    resp = resp['Ok']['$1']
    sys.stdout.write(json.dumps(resp))
else:
    resp = resp['Err']
    sys.stderr.write(json.dumps(resp))
"
}

call(){
    addr=$1
    port=$2
    method=$3
    args=$(join ${@:4})
    request="{ \"trace_context\": { \"trace_id\": 0, \"span_id\": 0 }, \"message\": { \"Request\": { \"id\": 0, \"message\": { \"$method\": [$args] }, \"deadline\": 0 } } }"
    echo "$request" | nc $addr $port | parseResponse $method
}
