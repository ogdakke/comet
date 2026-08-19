#!/bin/sh
# Fake pi CLI for zeron-harness tests.
#
# Emulates `pi --mode rpc`: newline-framed JSONL over stdio — commands in
# ({"id":N,"type":"get_state",...}), responses out ({"type":"response",
# "command":...,"id":N,"success":...}), agent events as bare objects.
# Scenario picked from the prompt text; driven by crates/harness/tests/pi.rs.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

# An agent event line.
ev() { emit "$1"; }

# A success response for request id $1 with data $2.
ok() { emit "{\"type\":\"response\",\"id\":$1,\"success\":true,\"data\":${2:-null}}"; }
# A failure response for request id $1 with error $2.
fail() { emit "{\"type\":\"response\",\"id\":$1,\"success\":false,\"error\":\"$2\"}"; }

# ---- scenario dispatch on the argv prompt is not available (prompt rides
# the RPC command), so state commands come first ----------------------------

# Resume runs pass --session <id>; the resume scenario verifies it landed.
RESUMED=0
case "$*" in
  *"--session pi-resume-9"*) RESUMED=1 ;;
esac

read -r line || exit 1
# models() discovery spawns the CLI with --no-session and asks directly —
# no get_state handshake first.
if has "$line" '"type":"get_available_models"'; then
  ok "$(rid "$line")" '{"models":[{"id":"claude-sonnet-4-5","name":"Claude Sonnet 4.5","provider":"anthropic","reasoning":true,"contextWindow":200000},{"id":"gemini-2.5-flash","name":"Gemini 2.5 Flash","provider":"google","reasoning":false,"contextWindow":1000000}]}'
  exit 0
fi
if has "$line" '"type":"get_commands"'; then
  ok "$(rid "$line")" '{"commands":[{"name":"fix-tests","description":"Fix failing tests","source":"prompt"},{"name":"skill:brave-search","description":"Web search","source":"skill"},{"name":"no-description","source":"extension"}]}'
  exit 0
fi
has "$line" '"type":"get_state"' || exit 1
STATE_ID=$(rid "$line")
ok "$STATE_ID" '{"sessionId":"pi-s-1","model":{"id":"claude-sonnet-4-5","name":"Claude Sonnet 4.5","provider":"anthropic"},"thinkingLevel":"medium","isStreaming":false}'

# ---- optional set_model / set_thinking_level, then the first prompt ------
SET_MODEL=""
SET_THINKING=""
read -r line || exit 1
while :; do
  if has "$line" '"type":"set_model"'; then
    SET_MODEL="$line"
    ok "$(rid "$line")" '{"id":"claude-sonnet-4-5","name":"Claude Sonnet 4.5","provider":"anthropic"}'
    read -r line || exit 1
  elif has "$line" '"type":"set_thinking_level"'; then
    SET_THINKING="$line"
    ok "$(rid "$line")"
    read -r line || exit 1
  else
    break
  fi
done

has "$line" '"type":"prompt"' || exit 1
PROMPT_ID=$(rid "$line")
# The prompt response means ACCEPTED — emitted before the turn runs.
ok "$PROMPT_ID"

case "$line" in

*scenario:basic*)
  ev '{"type":"agent_start"}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"thinking about it "}}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"more"}}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello "}}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"world"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hello world"}],"stopReason":"stop","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:tools*)
  ev '{"type":"tool_execution_start","toolCallId":"t1","toolName":"bash","args":{"command":"cargo test"}}'
  ev '{"type":"tool_execution_update","toolCallId":"t1","toolName":"bash","args":{"command":"cargo test"},"partialResult":{"content":[{"type":"text","text":"running"}]}}'
  ev '{"type":"tool_execution_end","toolCallId":"t1","toolName":"bash","result":{"content":[{"type":"text","text":"test result: ok"}]},"isError":false}'
  ev '{"type":"tool_execution_start","toolCallId":"t2","toolName":"read","args":{"path":"/w/src/main.rs"}}'
  ev '{"type":"tool_execution_end","toolCallId":"t2","toolName":"read","result":{"content":[{"type":"text","text":"fn main() {}"}]},"isError":false}'
  ev '{"type":"tool_execution_start","toolCallId":"t3","toolName":"edit","args":{"path":"/w/src/main.rs","oldText":"fn main() {}","newText":"fn main() { println!(\"hi\"); }"}}'
  ev '{"type":"tool_execution_end","toolCallId":"t3","toolName":"edit","result":{"content":[{"type":"text","text":"ok"}],"details":{"diff":"...display diff..."}},"isError":false}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","usage":{"input":20,"output":8}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:error*)
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"partial "}}'
  # A failed attempt that pi retries in-turn (its usage still burned)...
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"partial"}],"stopReason":"error","errorMessage":"overloaded","usage":{"input":5,"output":2}}}'
  ev '{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":100,"errorMessage":"overloaded"}'
  # ...and the retry succeeds: the clean message_end must supersede the
  # error, and usage must ACCUMULATE across both attempts (5+7, 2+3).
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"recovered"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"recovered"}],"stopReason":"stop","usage":{"input":7,"output":3}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:final-error*)
  ev '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"API key invalid","usage":{"input":1,"output":1}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:follow-up*)
  # A follow_up steer must NOT ride pi's steer command: the turn streams
  # and settles untouched; the parked message arrives as the next prompt.
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"one"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"one"}],"stopReason":"stop","usage":{"input":3,"output":1}}}'
  # Give the harness a beat to (wrongly) send a steer; a well-behaved
  # harness parks it and says nothing until the settle lands.
  sleep 1
  ev '{"type":"agent_settled"}'
  read -r line2 || exit 1
  has "$line2" '"type":"prompt"' || exit 1
  has "$line2" '"message":"queued for later"' || exit 1
  ok "$(rid "$line2")"
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"second turn"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"second turn"}],"stopReason":"stop","usage":{"input":4,"output":2}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:steer-race*)
  # The steer command loses the settle race: pi already settled and answers
  # "not streaming". The harness must queue it and deliver it as the next
  # prompt when agent_settled lands (it arrives right after the rejection).
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"quick"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"quick"}],"stopReason":"stop","usage":{"input":3,"output":1}}}'
  # The steer arrives after settle in the harness's view is still pending
  # (events channel lag): pi rejects it.
  read -r steerline || exit 1
  has "$steerline" '"type":"steer"' || exit 1
  fail "$(rid "$steerline")" "not streaming"
  # NOW the settle event lands; the queued steer becomes the next prompt.
  ev '{"type":"agent_settled"}'
  read -r line2 || exit 1
  has "$line2" '"type":"prompt"' || exit 1
  has "$line2" '"message":"late steer"' || exit 1
  ok "$(rid "$line2")"
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"recovered turn"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"recovered turn"}],"stopReason":"stop","usage":{"input":4,"output":2}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:steer*)
  # Turn 1 streams; the steer command arrives mid-turn (after the harness
  # saw tool_execution_start) and is answered ok; the settled turn follows.
  ev '{"type":"agent_start"}'
  ev '{"type":"tool_execution_start","toolCallId":"t1","toolName":"bash","args":{"command":"sleep 5"}}'
  # The steer lands while the tool runs (the harness sent it because the
  # turn is active).
  read -r steerline || exit 1
  has "$steerline" '"type":"steer"' || exit 1
  has "$steerline" '"message":"change course"' || exit 1
  ok "$(rid "$steerline")"
  ev '{"type":"tool_execution_end","toolCallId":"t1","toolName":"bash","result":{"content":[{"type":"text","text":""}]},"isError":false}'
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"steered reply"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"steered reply"}],"stopReason":"stop","usage":{"input":9,"output":4}}}'
  ev '{"type":"agent_settled"}'
  # The harness stays alive; a queued steer lost the race and arrives as a
  # new prompt on the same session.
  read -r line2 || exit 1
  has "$line2" '"type":"prompt"' || exit 1
  has "$line2" '"message":"after the boundary"' || exit 1
  ok "$(rid "$line2")"
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"second turn"}}'
  ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"second turn"}],"stopReason":"stop","usage":{"input":9,"output":4}}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:interrupt*)
  ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"working "}}'
  # The abort arrives mid-turn; answer it, then settle with an aborted
  # message (pi's real behavior: abort → agent_settled).
  read -r abortline || exit 1
  has "$abortline" '"type":"abort"' || exit 1
  ok "$(rid "$abortline")"
  ev '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"aborted"}}'
  ev '{"type":"agent_settled"}'
  ;;

*scenario:configured*)
  # The tests' request carries model anthropic/claude-opus-4-5 + high
  # reasoning; both commands must have arrived before the prompt.
  if has "$SET_MODEL" '"provider":"anthropic"' && has "$SET_MODEL" '"modelId":"claude-opus-4-5"' \
    && has "$SET_THINKING" '"level":"high"'; then
    ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"configured"}}'
    ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"configured"}],"stopReason":"stop","usage":{"input":6,"output":2}}}'
    ev '{"type":"agent_settled"}'
  else
    ev '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"setup commands missing"}}'
    ev '{"type":"agent_settled"}'
  fi
  ;;

*scenario:crash*)
  # Die mid-turn without settling: stdout EOF must surface as Errored with
  # the crash message.
  echo "fatal: pi exploded" >&2
  exit 42
  ;;

*scenario:resume*)
  # Only a resumed run (argv carried --session pi-resume-9) answers.
  if [ "$RESUMED" = "1" ]; then
    ev '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"resumed"}}'
    ev '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"resumed"}],"stopReason":"stop","usage":{"input":2,"output":1}}}'
    ev '{"type":"agent_settled"}'
  else
    echo "fatal: no --session arg" >&2
    exit 9
  fi
  ;;

*)
  exit 1
  ;;
esac
