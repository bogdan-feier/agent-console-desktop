#!/bin/sh
# Stands in for the real `claude` program in tests.
# It records how it was called (into the project folder it runs in) and prints
# the same kind of JSON lines the real CLI prints. The file `.fake-mode` in the
# project folder picks the scenario.

mode=$(cat .fake-mode 2>/dev/null || echo ok)
printf '%s\n' "$@" > fake-args.txt
cat > fake-prompt.txt
if [ -n "$ANTHROPIC_API_KEY" ]; then echo set > fake-key.txt; else echo unset > fake-key.txt; fi
echo $$ > fake-pid.txt

init='{"type":"system","subtype":"init","model":"claude-fake-1","session_id":"sess-1","tools":["Read"]}'

case "$mode" in
ok)
    echo '{"type":"autocompact_state","value":{"enabled":true}}'
    echo "$init"
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Reading the project."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/a.py"}}]}}'
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"rm -rf build"}}]}}'
    echo '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"Permission to use Bash was denied","is_error":true}]}}'
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t3","name":"StructuredOutput","input":{}}]}}'
    echo 'this line is not JSON and must be ignored'
    echo '{"type":"result","subtype":"success","is_error":false,"result":"All done.","structured_output":{"summary":"One task.","tasks":[{"title":"Do it","instructions":"Change a.py","files":["src/a.py"]}]},"session_id":"sess-1","total_cost_usd":0.42,"num_turns":3,"usage":{"input_tokens":1200,"cache_read_input_tokens":45000,"cache_creation_input_tokens":3000,"output_tokens":800}}'
    ;;
auth)
    echo "$init"
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Authentication error"}]},"error":"authentication_failed"}'
    echo '{"type":"result","subtype":"success","is_error":true,"result":"Authentication error","session_id":"sess-1","total_cost_usd":0,"num_turns":1}'
    exit 1
    ;;
budget)
    echo "$init"
    echo '{"type":"result","subtype":"error_max_budget_usd","is_error":true,"session_id":"sess-1","total_cost_usd":5.01,"num_turns":40}'
    exit 1
    ;;
maxturns)
    echo "$init"
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"big.py"}}]}}'
    echo '{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"sess-7","total_cost_usd":1.5,"num_turns":30,"usage":{"input_tokens":900,"output_tokens":4000}}'
    exit 1
    ;;
crash)
    echo "error: unknown option '--nope'" >&2
    exit 1
    ;;
hang)
    echo "$init"
    # A child process of its own, like a long test run started by a tool call.
    sleep 30 &
    echo $! > fake-child-pid.txt
    wait
    ;;
esac
