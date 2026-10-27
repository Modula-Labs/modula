#!/usr/bin/env bash
# End-to-end server migration with real installs: a main server installed by
# hand, a fake Hetzner Cloud (hetzner-mock.py) whose "servers" are more
# systemd containers on the e2e network running their cloud-init user data,
# and the dashboard API driving everything:
#
#   A. a provisioned worker running Postgres (with data) and a web service is
#      migrated to a new server in another location; the old one is deleted;
#   B. the database moves to another existing server (the main) on its own;
#   C. the main server itself is migrated: the control plane moves.
#
#   ARCHES=arm64 scripts/release.sh && deploy/e2e/migrate.sh   # keep: KEEP=1
#   SCENARIOS="A B" deploy/e2e/migrate.sh                       # skip C
set -euo pipefail
cd "$(dirname "$0")/../.."

# shellcheck source=deploy/e2e/lib.sh
. deploy/e2e/lib.sh

SCENARIOS=${SCENARIOS:-A B C}
MOCK=e2e-mock
PASSWORD="e2e-Password-$(openssl rand -hex 6)"
EMAIL=ops@e2e.test
STATUS_FILE=$(mktemp)

cleanup() {
  local rc=$?
  if ((rc != 0)); then
    echo "--- FAILED; mock log tail:" >&2
    docker logs --tail 30 "$MOCK" >&2 2>/dev/null || true
  fi
  [[ -n ${KEEP:-} ]] && { echo "kept: docker ps --filter label=modula-e2e=1; docker exec -it e2e-main bash"; return; }
  local c
  for c in $(docker ps -aq --filter label=modula-e2e=1) ; do
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
  docker rm -f e2e-main "$MOCK" >/dev/null 2>&1 || true
  for c in $(docker volume ls -q | grep -E '^e2e-.*-(docker|containerd)$'); do
    docker volume rm "$c" >/dev/null 2>&1 || true
  done
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap 'cleanup; rm -f "$STATUS_FILE" "$STATUS_FILE.sudo"' EXIT

# wait_until SECONDS DESCRIPTION COMMAND… — until COMMAND succeeds.
wait_until() {
  local secs=$1 what=$2 t
  shift 2
  t=$(($(date +%s) + secs))
  until "$@"; do
    (($(date +%s) < t)) || fail "timed out after ${secs}s: $what"
    sleep 2
  done
}

mock() { # mock METHOD PATH [JSON]
  docker exec "$MOCK" curl -fsS -X "$1" -H 'Content-Type: application/json' ${3:+--data "$3"} "http://127.0.0.1:8080$2"
}

# The dashboard API as a browser would use it, from the mock's container:
# https to the dashboard domain at the main server's address (Caddy's internal
# CA), a cookie session, the dashboard's Origin. Sets STATUS; body on stdout.
MAIN_IP=""
STATUS=""
api() { # api METHOD PATH [JSON]
  local out
  out=$(docker exec "$MOCK" curl -sk --max-time 60 -X "$1" --resolve "$DOMAIN:443:$MAIN_IP" \
    -b /tmp/jar -c /tmp/jar -H "Origin: https://$DOMAIN" -H 'Content-Type: application/json' \
    ${3:+--data "$3"} -w '\n%{http_code}' "https://$DOMAIN$2" 2>&1) || true
  printf '%s' "${out##*$'\n'}" >"$STATUS_FILE"
  printf '%s' "${out%$'\n'*}"
}
# ok METHOD PATH [JSON] — api, failing unless 2xx.
ok() {
  local body
  body=$(api "$@")
  STATUS=$(<"$STATUS_FILE")
  [[ $STATUS == 2* ]] || fail "$1 $2 → $STATUS: $body"
  printf '%s' "$body"
}
# Sudo once per session (it lasts; asking again is rate limited).
sudo_() {
  [[ -e $STATUS_FILE.sudo ]] && return 0
  ok POST /api/auth/sudo "{\"password\":\"$PASSWORD\"}" >/dev/null && touch "$STATUS_FILE.sudo"
}

psql_on() { # psql_on CONTAINER SQL — the control plane's database
  on "$1" runuser -u postgres -- psql -d modula -tAc "$2"
}

# The database's rows through the dashboard's query API.
sql() { # sql INSTANCE SQL
  sudo_
  ok POST "/api/service-instances/$1/database/query" "$(jq -nc --arg s "$2" '{sql: $s}')"
}

instance_on() { # instance_on INSTANCE NODE — online there
  local i
  i=$(api GET "/api/service-instances/$1")
  [[ $(jq -r .nodeId <<<"$i") == "$2" && $(jq -r .status <<<"$i") == online ]]
}

# The web service answers through Caddy on the server it runs on.
serves() { # serves HOST IP
  docker exec "$MOCK" curl -sk --max-time 10 --resolve "$1:443:$2" "https://$1/" 2>/dev/null | grep -q "Hostname"
}

container_of() { # the mock's container for a provider server id
  mock GET /mock/servers | jq -r --arg id "$1" '.servers[] | select((.id|tostring) == $id) | .container'
}

# ---------------------------------------------------------------- setup

build_image
docker build -q -t modula-e2e-hetzner-mock -f deploy/e2e/hetzner-mock.Dockerfile deploy/e2e >/dev/null
docker rm -f "$MOCK" >/dev/null 2>&1 || true
docker run -d --name "$MOCK" --network "$NET" --cap-add NET_ADMIN \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -e NET="$NET" -e RELEASE_HOST="$RELEASE" -e FILES_HOST="$FILES" -e VERSION="$VERSION" -e DOMAIN="$DOMAIN" \
  modula-e2e-hetzner-mock >/dev/null
wait_until 30 "the mock answers" mock GET /mock/servers >/dev/null
echo "fake Hetzner at $(ip_of "$MOCK"):8080"

echo "main server"
boot e2e-main
MAIN=e2e-main
MAIN_IP=$(ip_of e2e-main)
mock POST /mock/prep '{"container":"e2e-main"}' >/dev/null
on e2e-main sh -c "echo '127.0.0.1 $DOMAIN' >> /etc/hosts"
setup_token="mcsetup_$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')"
on "${env_vars[@]}" -e NO_COLOR=1 e2e-main bash /root/install.sh --domain "$DOMAIN" --email "$EMAIL" \
  --apps-domain apps.e2e.test --setup-token "$setup_token" >/tmp/modula-e2e-install.log 2>&1 \
  || { tail -30 /tmp/modula-e2e-install.log >&2; fail "main install failed"; }
pass "main server installed"
mock POST /mock/main '{"container":"e2e-main"}' >/dev/null
mock POST /mock/register '{"name":"e2e-main","container":"e2e-main","serverType":"cax21","location":"fsn1","labels":{"modula":"1","role":"main"}}' >/dev/null

wait_until 60 "the dashboard over https" sh -c "docker exec $MOCK curl -sk --resolve $DOMAIN:443:$MAIN_IP -o /dev/null -f https://$DOMAIN/api/health"
setup=$(ok POST /api/auth/setup "$(jq -nc --arg t "$setup_token" --arg e "$EMAIL" --arg p "$PASSWORD" \
  '{setupToken: $t, email: $e, name: "Ops", password: $p, workspaceName: "E2E"}')")
WS=$(jq -r '.workspaces[0].id' <<<"$setup")
[[ $WS =~ ^[0-9a-zA-Z_-]+$ ]] || fail "no workspace: $setup"
pass "admin set up over https (workspace $WS)"

main_node=$(psql_on e2e-main "SELECT id FROM nodes WHERE status = 'online' LIMIT 1")
wait_until 60 "the main node reports its container address" \
  sh -c "[ \"\$(docker exec e2e-main runuser -u postgres -- psql -d modula -tAc \"SELECT public_ip FROM nodes WHERE id = '$main_node'\")\" = $MAIN_IP ]"
pass "main node's public IP is its container address ($MAIN_IP)"

sudo_
ok PUT "/api/workspaces/$WS/integrations/hetzner" '{"token":"e2e-dummy-hetzner-token-0123456789abcdef"}' >/dev/null
ok PATCH "/api/workspaces/$WS/integrations/hetzner" '{"maxServers":10,"maxMonthlyCents":100000}' >/dev/null || true
pass "Hetzner connected (fake API)"

project=$(ok POST "/api/workspaces/$WS/projects" '{"name":"shop"}' | jq -r .id)

# ---------------------------------------------------------------- A

provision() { # provision NAME LOCATION TYPE — waits until ready; prints the server row
  local s id
  sudo_
  s=$(ok POST "/api/workspaces/$WS/servers/provision" "{\"name\":\"$1\",\"location\":\"hetzner:$2\",\"serverType\":\"$3\"}")
  id=$(jq -r .id <<<"$s")
  local t=$(($(date +%s) + 900))
  while :; do
    s=$(ok GET "/api/servers/$id")
    case $(jq -r .status <<<"$s") in
      ready) break ;;
      failed) fail "$1 failed: $(jq -r .error <<<"$s")" ;;
    esac
    (($(date +%s) < t)) || fail "$1 did not join in 900s ($(jq -r .status <<<"$s"))"
    sleep 3
  done
  printf '%s' "$s"
}

if [[ $SCENARIOS == *A* ]]; then
  echo "A. worker migration"
  w1=$(provision w1 fsn1 cax11)
  w1_node=$(jq -r .nodeId <<<"$w1")
  w1_pid=$(jq -r .providerId <<<"$w1")
  w1_c=$(container_of "$w1_pid")
  pass "provisioned w1 through the fake cloud ($w1_c, node $w1_node)"

  pg=$(ok POST "/api/projects/$project/services" '{"name":"postgres","template":"postgres"}')
  PG=$(jq -r '.instances[0].id' <<<"$pg")
  # Placed with a move: the plain pin refuses services with a volume.
  sudo_
  pmv=$(ok POST "/api/service-instances/$PG/move" "{\"nodeId\":\"$w1_node\"}" | jq -r .id)
  wait_until 300 "postgres placed on w1" sh -c "[ \"\$(docker exec $MAIN runuser -u postgres -- psql -d modula -tAc \"SELECT status FROM instance_moves WHERE id = '$pmv'\")\" = done ]"
  web=$(ok POST "/api/projects/$project/services" '{"name":"web","config":{"source":{"type":"image","image":"traefik/whoami:v1.10"},"port":80}}')
  WEB=$(jq -r '.instances[0].id' <<<"$web")
  # shellcheck disable=SC2016 # Modula references, not shell
  ok PATCH "/api/service-instances/$WEB/variables" '{"set":{"DB_HOST":"${{postgres.PGHOST}}","DB_URL":"${{postgres.DATABASE_URL}}"}}' >/dev/null
  ok PATCH "/api/service-instances/$WEB/server" "{\"nodeId\":\"$w1_node\"}" >/dev/null
  WEB_HOST=$(ok POST "/api/service-instances/$WEB/domains" '{}' | jq -r '.hostname // .domain // .host')
  env_id=$(jq -r '.instances[0].environmentId' <<<"$pg")
  api POST "/api/environments/$env_id/deploy" '{}' >/dev/null
  for i in "$PG" "$WEB"; do
    [[ $(ok GET "/api/service-instances/$i/deployments" | jq 'length') -gt 0 ]] || ok POST "/api/service-instances/$i/deployments" '{}' >/dev/null
  done
  wait_until 600 "postgres online on w1" instance_on "$PG" "$w1_node"
  wait_until 300 "web online on w1" instance_on "$WEB" "$w1_node"
  pass "postgres and web run on w1"
  w1_ip=$(ip_of "$w1_c")
  wait_until 120 "web serves $WEB_HOST on w1" serves "$WEB_HOST" "$w1_ip"
  pass "web serves https://$WEB_HOST on w1"

  t=$(($(date +%s) + 180))
  until sql "$PG" "CREATE TABLE IF NOT EXISTS e2e (id int primary key, note text)" >/dev/null 2>&1; do
    (($(date +%s) < t)) || fail "cannot create a table: $(sql "$PG" "SELECT 1" || true)"
    sleep 3
  done
  sql "$PG" "INSERT INTO e2e VALUES (1, 'written on w1') ON CONFLICT DO NOTHING" >/dev/null
  sql "$PG" "SELECT note FROM e2e WHERE id = 1" | grep -q "written on w1" && pass "wrote a row" || fail "row not written"

  preview=$(ok GET "/api/nodes/$w1_node/migration-preview")
  [[ $(jq '.services | length' <<<"$preview") -ge 2 ]] && pass "migration preview lists $(jq -c '[.services[].service]' <<<"$preview")" \
    || fail "preview: $preview"
  sudo_
  mig=$(ok POST "/api/nodes/$w1_node/migrate" '{"location":"hetzner:hel1","serverType":"cax11"}')
  MIG=$(jq -r .id <<<"$mig")
  t=$(($(date +%s) + 1500))
  while :; do
    m=$(ok GET "/api/server-migrations/$MIG")
    st=$(jq -r .status <<<"$m")
    [[ $st == "done" ]] && break
    [[ $st == running ]] || fail "migration $st: $(jq -r .error <<<"$m"); steps: $(jq -c .steps <<<"$m")"
    (($(date +%s) < t)) || fail "migration stuck at $(jq -r .step <<<"$m"): $(jq -c .steps <<<"$m")"
    sleep 5
  done
  w2_node=$(jq -r .targetNodeId <<<"$m")
  w2_sid=$(jq -r .targetServerId <<<"$m")
  w2=$(ok GET "/api/servers/$w2_sid")
  w2_c=$(container_of "$(jq -r .providerId <<<"$w2")")
  w2_ip=$(ip_of "$w2_c")
  pass "migration done: $(jq -r .targetName <<<"$m") ($w2_c) in $(jq -r .location.city <<<"$w2"), moves: $(jq -c '[.moves[] | .service + ":" + .status]' <<<"$m")"
  instance_on "$PG" "$w2_node" && pass "postgres runs on the new server" || fail "postgres not on the new server"
  instance_on "$WEB" "$w2_node" && pass "web runs on the new server" || fail "web not on the new server"
  sql "$PG" "SELECT note FROM e2e WHERE id = 1" | grep -q "written on w1" && pass "the row survived the migration" || fail "data lost"
  wait_until 120 "web serves on the new server" serves "$WEB_HOST" "$w2_ip"
  pass "web serves https://$WEB_HOST on the new server"
  [[ $(jq -r .sourcePendingDeletion <<<"$m") == true ]] && pass "w1 is pending deletion" || fail "w1 not pending_deletion"
  sudo_
  ok DELETE "/api/servers/$(jq -r .id <<<"$w1")" >/dev/null
  docker inspect "$w1_c" >/dev/null 2>&1 && fail "w1's container still exists" || pass "w1 deleted: its container is gone"
  [[ $(psql_on "$MAIN" "SELECT status FROM nodes WHERE id = '$w1_node'") == revoked ]] && pass "w1's node is revoked" || fail "w1's node not revoked"
fi

# ---------------------------------------------------------------- B

if [[ $SCENARIOS == *B* ]]; then
  echo "B. one service moves to an existing server"
  [[ -n ${PG:-} ]] || fail "scenario B needs A's database"
  sudo_
  mv=$(ok POST "/api/service-instances/$PG/move" "{\"nodeId\":\"$main_node\"}")
  MV=$(jq -r .id <<<"$mv")
  t=$(($(date +%s) + 900))
  while :; do
    m=$(ok GET "/api/instance-moves/$MV")
    st=$(jq -r .status <<<"$m")
    [[ $st == "done" ]] && break
    [[ $st == running || $st == pending ]] || fail "move $st: $(jq -r .error <<<"$m")"
    (($(date +%s) < t)) || fail "move stuck at $(jq -r .phase <<<"$m")"
    sleep 3
  done
  pass "move done ($(jq -r .copiedBytes <<<"$m") bytes copied)"
  instance_on "$PG" "$main_node" && pass "postgres runs on the main server" || fail "postgres not on main"
  sql "$PG" "SELECT note FROM e2e WHERE id = 1" | grep -q "written on w1" && pass "the row survived the move" || fail "data lost in the move"
  sql "$PG" "INSERT INTO e2e VALUES (2, 'written on main') ON CONFLICT DO NOTHING" >/dev/null
fi

# ---------------------------------------------------------------- C

if [[ $SCENARIOS == *C* ]]; then
  echo "C. main server migration"
  preview=$(ok GET "/api/nodes/$main_node/migration-preview")
  [[ $(jq -r .main <<<"$preview") == true && $(jq -r .mainBlocker <<<"$preview") == null && $(jq -r .serverId <<<"$preview") != null ]] \
    && pass "the main server is linked to its fake Hetzner server" || fail "main preview: $preview"
  old_main_sid=$(jq -r .serverId <<<"$preview")
  sudo_
  mig=$(ok POST "/api/nodes/$main_node/migrate" '{"location":"hetzner:nbg1","serverType":"cax21"}')
  MIG=$(jq -r .id <<<"$mig")
  new_sid=""
  t=$(($(date +%s) + 2400))
  switched=""
  while :; do
    m=$(api GET "/api/server-migrations/$MIG")
    STATUS=$(<"$STATUS_FILE")
    if [[ $STATUS == 2* ]]; then
      st=$(jq -r .status <<<"$m")
      [[ -n $new_sid ]] || new_sid=$(jq -r '.targetServerId // empty' <<<"$m")
      [[ $st == "done" ]] && break
      [[ $st == running ]] || fail "main migration $st: $(jq -r .error <<<"$m"); steps: $(jq -c .steps <<<"$m")"
    elif [[ -z $switched && $STATUS == 503 || -z $switched && $STATUS == 307 || -z $switched && $STATUS == 308 ]]; then
      echo "  … old control plane answers $STATUS: $(head -c 200 <<<"$m")"
      new_c=$(mock GET /mock/servers | jq -r '.servers[] | select(.labels["modula-id"] != null and .location.name == "nbg1") | .container')
      [[ -n $new_c ]] || fail "no new main container at the fake cloud"
      MAIN_IP=$(ip_of "$new_c")
      MAIN=$new_c
      switched=1
      # The dashboard's DNS (no Cloudflare here): point the domain at the new server.
      mock POST /mock/main "{\"container\":\"$new_c\"}" >/dev/null
      for c in $(docker ps --filter label=modula-e2e=1 --format '{{.Names}}') e2e-main; do
        [[ $c == "$new_c" ]] && continue
        on "$c" sh -c "grep -v ' $DOMAIN\$' /etc/hosts >/tmp/h; echo '$MAIN_IP $DOMAIN' >>/tmp/h; cat /tmp/h >/etc/hosts"
      done
      on "$new_c" sh -c "grep -q ' $DOMAIN\$' /etc/hosts || echo '127.0.0.1 $DOMAIN' >>/etc/hosts"
    else
      [[ -n $switched ]] || fail "old control plane answers $STATUS: $m"
    fi
    (($(date +%s) < t)) || fail "main migration stuck: $(jq -c '{step,promotePhase,promoteDetail,steps}' <<<"$m" 2>/dev/null || echo "$m")"
    sleep 5
  done
  [[ -n $switched ]] || fail "the migration finished without the old control plane retiring"
  pass "main migration done; new main $MAIN ($MAIN_IP): $(jq -c '[.steps[] | .label + ":" + .status]' <<<"$m")"
  api POST /api/auth/logout '{}' >/dev/null || true
  docker exec "$MOCK" rm -f /tmp/jar
  rm -f "$STATUS_FILE.sudo"
  ok POST /api/auth/login "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" | jq -e '.me' >/dev/null \
    && pass "log in on the new control plane with the same credentials" || fail "login failed"
  ok GET "/api/workspaces/$WS/projects" | jq -e --arg p "$project" '.. | objects | select(.id? == $p)' >/dev/null \
    && pass "the project is there" || fail "project missing"
  old=$(on e2e-main curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:7100/api/health || true)
  [[ $old == 503 ]] && pass "the old control plane is retired (503)" || fail "old control plane answers $old"
  new_node=$(jq -r .targetNodeId <<<"$m")
  wait_until 120 "the new main's node is online" sh -c "[ \"\$(docker exec $MAIN runuser -u postgres -- psql -d modula -tAc \"SELECT status FROM nodes WHERE id = '$new_node'\")\" = online ]"
  [[ $(psql_on "$MAIN" "SELECT status FROM nodes WHERE id = '$main_node'") == revoked ]] && pass "the old main's node is revoked" || fail "old main node not revoked"
  if [[ -n ${w2_c:-} ]]; then
    wait_until 180 "w2 connects to the new main" sh -c "docker exec $w2_c grep -q 'https://$MAIN_IP:' /var/lib/modula-node/node.json"
    wait_until 120 "w2 online at the new main" sh -c "[ \"\$(docker exec $MAIN runuser -u postgres -- psql -d modula -tAc \"SELECT status FROM nodes WHERE id = '$w2_node'\")\" = online ]"
    pass "w2 switched to the new main"
    wait_until 120 "web still serves" serves "$WEB_HOST" "$w2_ip"
    pass "web still serves https://$WEB_HOST"
  fi
  if [[ -n ${PG:-} ]]; then
    wait_until 600 "postgres online on the new main" instance_on "$PG" "$new_node"
    sql "$PG" "SELECT string_agg(note, ',' ORDER BY id) FROM e2e" | grep -q "written on w1,written on main" \
      && pass "both rows survived the main migration" || fail "data lost in the main migration"
  fi
  [[ $(jq -r .sourcePendingDeletion <<<"$m") == true ]] && pass "the old main is pending deletion" || fail "old main not pending_deletion"
  sudo_
  ok DELETE "/api/servers/$old_main_sid" >/dev/null
  docker inspect e2e-main >/dev/null 2>&1 && fail "the old main's container still exists" || pass "old main deleted from the new dashboard: its container is gone"
fi

echo "PASS: migration e2e ($SCENARIOS)"
