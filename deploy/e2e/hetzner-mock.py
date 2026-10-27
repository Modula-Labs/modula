"""A fake Hetzner Cloud for deploy/e2e/migrate.sh.

Serves the part of https://api.hetzner.cloud/v1 Modula uses (locations,
server types, servers, actions, firewalls, SSH keys). A created server is a
privileged systemd container from the e2e image on the e2e network: the
mock boots it, prepares it like a cloud image would be (see `prep`), and runs
the request's user data in it as cloud-init would, with the output in
/var/log/cloud-init-output.log. Its public IPv4 is its container address.

Also, on 1.1.1.1:443 (this container owns that address; the e2e servers
route it here), Cloudflare's /cdn-cgi/trace answering each server with its
own container address, so modula-node reports that as its public IP.

Test-only endpoints under /mock/: register an existing container as a
server (the main server, installed by hand), prepare a container, set which
container is the main server (whose name the e2e domain resolves to), list.
"""

import itertools
import json
import os
import re
import ssl
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

NET = os.environ["NET"]
IMAGE = os.environ.get("IMAGE", "modula-e2e-ubuntu")
RELEASE = os.environ["RELEASE_HOST"]
FILES = os.environ["FILES_HOST"]
VERSION = os.environ["VERSION"]
DOMAIN = os.environ["DOMAIN"]
PREFIX = os.environ.get("CONTAINER_PREFIX", "e2e-")
PORT = 8080

LOCATIONS = [
    {"id": 1, "name": "fsn1", "description": "Falkenstein DC Park 1", "country": "DE",
     "city": "Falkenstein", "network_zone": "eu-central"},
    {"id": 2, "name": "nbg1", "description": "Nuremberg DC Park 1", "country": "DE",
     "city": "Nuremberg", "network_zone": "eu-central"},
    {"id": 3, "name": "hel1", "description": "Helsinki DC Park 1", "country": "FI",
     "city": "Helsinki", "network_zone": "eu-central"},
]


def _type(tid, name, cores, memory, disk, gross):
    return {
        "id": tid, "name": name, "description": name.upper(), "cores": cores, "memory": memory,
        "disk": disk, "cpu_type": "shared", "architecture": "arm", "storage_type": "local",
        "deprecated": False, "deprecation": None,
        "prices": [{"location": l["name"],
                    "price_hourly": {"net": "0.0100", "gross": "0.0100"},
                    "price_monthly": {"net": gross, "gross": gross},
                    "included_traffic": 21990232555520} for l in LOCATIONS],
        "locations": [{"id": l["id"], "name": l["name"], "deprecation": None, "available": True}
                      for l in LOCATIONS],
    }


TYPES = [_type(45, "cax11", 2, 4.0, 40, "4.5100"), _type(93, "cax21", 4, 8.0, 80, "7.4900")]

lock = threading.RLock()
servers = {}  # id -> dict (API shape plus "_container")
firewalls = {}
actions = {}
ids = itertools.count(1001)
main = {"container": None, "ip": None}


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def sh(*args, input=None, check=True):
    r = subprocess.run(args, input=input, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"{args[:4]} failed: {r.stderr.strip()[-400:]}")
    return r.stdout.strip()


def my_ip():
    out = sh("ip", "-4", "-o", "addr", "show", "eth0")
    return re.search(r"inet (\d+\.\d+\.\d+\.\d+)", out).group(1)


MY_IP = None


def container_ip(c):
    return sh("docker", "inspect", "-f",
              "{{(index .NetworkSettings.Networks \"%s\").IPAddress}}" % NET, c)


def action(command, status="success"):
    a = {"id": next(ids), "command": command, "status": status, "progress": 100,
         "started": now(), "finished": now(), "resources": [], "error": None}
    actions[a["id"]] = a
    return a


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())


def view(s):
    return {k: v for k, v in s.items() if not k.startswith("_")}


def matches(labels, selector):
    for term in filter(None, (t.strip() for t in selector.split(","))):
        if "!=" in term:
            k, v = term.split("!=", 1)
            if labels.get(k) == v:
                return False
        elif "=" in term:
            k, v = term.split("=", 1)
            if labels.get(k) != v:
                return False
        elif term.startswith("!"):
            if term[1:] in labels:
                return False
        elif term not in labels:
            return False
    return True


# ---------------------------------------------------------------- containers

def wait_systemd(c):
    for _ in range(300):
        r = subprocess.run(["docker", "exec", c, "systemctl", "is-system-running"],
                           capture_output=True, text=True)
        if re.search(r"running|degraded", r.stdout):
            return
        time.sleep(0.2)
    raise RuntimeError(f"{c}: systemd did not start")


def caddy_root(c):
    """The local CA Caddy on `c` signs internal certificates with."""
    return subprocess.run(
        ["docker", "exec", c, "sh", "-c",
         "f=$(find /var/lib /root /etc -path '*pki/authorities/local/root.crt' 2>/dev/null | head -1);"
         " [ -n \"$f\" ] && cat \"$f\""],
        capture_output=True, text=True).stdout


def prep(c):
    """What a cloud image would bring: the mock CA (and the main server's
    Caddy CA, standing in for a public one) trusted, the e2e domain
    resolving to the main server, 1.1.1.1 routed to this mock, and
    modula-server (should this become the main server) pointed at this mock
    with Caddy's internal CA (no ACME here)."""
    certs = {"e2e-mock.crt": open("/certs/ca.crt").read()}
    if main["container"] and main["container"] != c:
        root = caddy_root(main["container"])
        if root:
            certs["e2e-caddy-main.crt"] = root
    for name, pem in certs.items():
        sh("docker", "exec", "-i", c, "sh", "-c",
           f"cat >/usr/local/share/ca-certificates/{name}", input=pem)
    sh("docker", "exec", c, "update-ca-certificates")
    sh("docker", "exec", c, "ip", "route", "replace", "1.1.1.1/32", "via", MY_IP)
    if main["ip"] and main["container"] != c:
        set_hosts(c, main["ip"])
    # A second EnvironmentFile: it overrides server.env (which sets MODULA_EDGE_TLS).
    env = f"MODULA_HETZNER_API_URL=http://{MY_IP}:{PORT}/v1\nMODULA_EDGE_TLS=internal\n"
    sh("docker", "exec", "-i", c, "sh", "-c", "mkdir -p /etc/modula && cat >/etc/modula/e2e.env",
       input=env)
    sh("docker", "exec", "-i", c, "sh", "-c",
       "mkdir -p /etc/systemd/system/modula-server.service.d &&"
       " cat >/etc/systemd/system/modula-server.service.d/e2e.conf && systemctl daemon-reload",
       input="[Service]\nEnvironmentFile=/etc/modula/e2e.env\n")


def set_hosts(c, ip):
    sh("docker", "exec", c, "sh", "-c",
       f"grep -v ' {DOMAIN}$' /etc/hosts >/tmp/hosts; echo '{ip} {DOMAIN}' >>/tmp/hosts;"
       " cat /tmp/hosts >/etc/hosts")


def boot(server_id, user_data):
    with lock:
        c = servers[server_id]["_container"]
    try:
        wait_systemd(c)
        prep(c)
        with lock:
            if server_id in servers:
                servers[server_id]["status"] = "running"
        sh("docker", "exec", "-i", c, "sh", "-c",
           "mkdir -p /var/lib/cloud && cat >/var/lib/cloud/user-data && chmod 700 /var/lib/cloud/user-data",
           input=user_data)
        sh("docker", "exec", "-d", "-e", "MODULA_RELEASE_URL=file:///srv/releases",
           "-e", "NO_COLOR=1", c, "bash", "-c",
           "bash /var/lib/cloud/user-data >/var/log/cloud-init-output.log 2>&1;"
           " echo \"exit $?\" >>/var/log/cloud-init-output.log")
        log(f"{c}: running user data")
    except Exception as e:  # noqa: BLE001
        log(f"{c}: boot failed: {e}")


def create_container(c):
    subprocess.run(["docker", "rm", "-f", c], capture_output=True)
    sh("docker", "run", "-d", "--name", c, "--hostname", c, "--network", NET,
       "--label", "modula-e2e=1",
       "--privileged", "--cgroupns=host", "-v", "/sys/fs/cgroup:/sys/fs/cgroup:rw",
       "--tmpfs", "/run", "--tmpfs", "/run/lock",
       # Volumes, not the container's overlay: Docker's (and containerd's)
       # overlayfs can't stack on it.
       "-v", f"{c}-docker:/var/lib/docker", "-v", f"{c}-containerd:/var/lib/containerd",
       "-v", f"{RELEASE}:/srv/releases/download/v{VERSION}:ro",
       "-v", f"{FILES}:/srv/releases/files:ro",
       IMAGE)
    return container_ip(c)


def remove_container(c):
    subprocess.run(["docker", "rm", "-f", c], capture_output=True)
    for v in ("docker", "containerd"):
        subprocess.run(["docker", "volume", "rm", f"{c}-{v}"], capture_output=True)


def server_record(sid, name, c, ip, type_name, location, labels, status):
    ty = next(t for t in TYPES if t["name"] == type_name)
    loc = next(l for l in LOCATIONS if l["name"] == location)
    return {
        "id": sid, "name": name, "status": status, "created": now(),
        "public_net": {"ipv4": {"id": sid, "ip": ip, "blocked": False}, "ipv6": None,
                       "floating_ips": [], "firewalls": []},
        "private_net": [], "server_type": ty, "location": loc, "labels": labels,
        "primary_disk_size": ty["disk"], "_container": c,
    }


# ---------------------------------------------------------------- HTTP

class Api(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        log(self.command, self.path.split("?")[0], fmt % args if "%" in fmt else "")

    def reply(self, code, body=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def error(self, code, kind, message):
        self.reply(code, {"error": {"code": kind, "message": message}})

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}") if n else {}

    def page(self, key, items):
        self.reply(200, {key: items, "meta": {"pagination": {
            "page": 1, "per_page": 50, "previous_page": None, "next_page": None,
            "last_page": 1, "total_entries": len(items)}}})

    def authorized(self):
        if self.path.startswith("/mock/"):
            return True
        if not self.headers.get("Authorization", "").startswith("Bearer "):
            self.error(401, "unauthorized", "unable to authenticate")
            return False
        return True

    def route(self, method):
        if not self.authorized():
            return
        url = urlparse(self.path)
        q = parse_qs(url.query)
        p = url.path.rstrip("/")
        selector = q.get("label_selector", [""])[0]
        try:
            self.dispatch(method, p, selector)
        except Exception as e:  # noqa: BLE001
            log("error:", repr(e))
            self.error(500, "server_error", str(e))

    def dispatch(self, method, p, selector):
        m = re.fullmatch
        if method == "GET" and p == "/v1/locations":
            return self.page("locations", LOCATIONS)
        if method == "GET" and p == "/v1/server_types":
            return self.page("server_types", TYPES)
        if method == "GET" and p == "/v1/ssh_keys":
            return self.page("ssh_keys", [])
        if method == "GET" and p == "/v1/servers":
            with lock:
                return self.page("servers", [view(s) for s in servers.values()
                                             if matches(s["labels"], selector)])
        if method == "POST" and p == "/v1/servers":
            return self.create_server(self.body())
        if r := m(r"/v1/servers/(\d+)", p):
            sid = int(r.group(1))
            with lock:
                s = servers.get(sid)
                if not s:
                    return self.error(404, "not_found", "server not found")
                if method == "GET":
                    return self.reply(200, {"server": view(s)})
                if method == "DELETE":
                    s["status"] = "deleting"
                    c = s["_container"]
            if method == "DELETE":
                remove_container(c)
                with lock:
                    servers.pop(sid, None)
                log(f"deleted server {sid} ({c})")
                return self.reply(200, {"action": action("delete_server")})
        if r := m(r"/v1/servers/(\d+)/actions/(\w+)", p):
            return self.server_action(int(r.group(1)), r.group(2), self.body())
        if r := m(r"/v1/actions/(\d+)", p):
            a = actions.get(int(r.group(1)))
            return self.reply(200, {"action": a}) if a else self.error(404, "not_found", "no action")
        if method == "GET" and p == "/v1/firewalls":
            with lock:
                return self.page("firewalls", [f for f in firewalls.values()
                                               if matches(f["labels"], selector)])
        if method == "POST" and p == "/v1/firewalls":
            b = self.body()
            with lock:
                fw = {"id": next(ids), "name": b.get("name", ""), "labels": b.get("labels") or {},
                      "rules": b.get("rules") or [], "applied_to": []}
                firewalls[fw["id"]] = fw
            return self.reply(201, {"firewall": fw, "actions": []})
        if r := m(r"/v1/firewalls/(\d+)", p):
            with lock:
                fw = firewalls.get(int(r.group(1)))
                if not fw:
                    return self.error(404, "not_found", "firewall not found")
                if method == "DELETE":
                    firewalls.pop(fw["id"])
                    return self.reply(204)
                return self.reply(200, {"firewall": fw})
        if r := m(r"/v1/firewalls/(\d+)/actions/(set_rules|apply_to_resources)", p):
            b = self.body()
            with lock:
                fw = firewalls.get(int(r.group(1)))
                if not fw:
                    return self.error(404, "not_found", "firewall not found")
                if r.group(2) == "set_rules":
                    fw["rules"] = b.get("rules") or []
                else:
                    fw["applied_to"].extend(b.get("apply_to") or [])
            return self.reply(201, {"actions": [action(r.group(2))]})
        # ---- test-only
        if method == "GET" and p == "/mock/servers":
            with lock:
                return self.reply(200, {"servers": [dict(view(s), container=s["_container"])
                                                    for s in servers.values()],
                                        "firewalls": list(firewalls.values())})
        if method == "POST" and p == "/mock/prep":
            prep(self.body()["container"])
            return self.reply(200, {"ok": True, "mockIp": MY_IP})
        if method == "POST" and p == "/mock/main":
            c = self.body()["container"]
            ip = container_ip(c)
            with lock:
                main.update(container=c, ip=ip)
            return self.reply(200, {"ok": True, "ip": ip})
        if method == "POST" and p == "/mock/register":
            b = self.body()
            c = b["container"]
            sid = next(ids)
            with lock:
                servers[sid] = server_record(sid, b["name"], c, container_ip(c),
                                             b.get("serverType", "cax21"),
                                             b.get("location", "fsn1"), b.get("labels") or {},
                                             "running")
            return self.reply(200, {"id": sid})
        self.error(404, "not_found", f"no route {method} {p}")

    def create_server(self, b):
        for f in ("name", "server_type", "location", "image"):
            if not b.get(f):
                return self.error(422, "invalid_input", f"{f} is required")
        if not any(t["name"] == b["server_type"] for t in TYPES):
            return self.error(422, "invalid_input", "unknown server type")
        if not any(l["name"] == b["location"] for l in LOCATIONS):
            return self.error(422, "invalid_input", "unknown location")
        with lock:
            if any(s["name"] == b["name"] for s in servers.values()):
                return self.error(409, "uniqueness_error", "server name is already used")
        c = PREFIX + b["name"]
        ip = create_container(c)
        sid = next(ids)
        labels = b.get("labels") or {}
        with lock:
            s = server_record(sid, b["name"], c, ip, b["server_type"], b["location"], labels,
                              "initializing")
            servers[sid] = s
            for ref in b.get("firewalls") or []:
                fw = firewalls.get(ref.get("firewall"))
                if fw:
                    fw["applied_to"].append({"type": "server", "server": {"id": sid}})
            a = action("create_server", "running")
        threading.Thread(target=self.finish_create, args=(sid, a, b.get("user_data") or ""),
                         daemon=True).start()
        log(f"created server {sid} {b['name']} ({c}, {ip}) in {b['location']} labels={labels}")
        return self.reply(201, {"server": view(s), "action": a, "next_actions": [],
                                "root_password": None})

    @staticmethod
    def finish_create(sid, a, user_data):
        boot(sid, user_data)
        a["status"] = "success"

    def server_action(self, sid, name, b):
        with lock:
            s = servers.get(sid)
            if not s:
                return self.error(404, "not_found", "server not found")
            c = s["_container"]
        if name in ("shutdown", "poweroff"):
            sh("docker", "stop", "-t", "20" if name == "shutdown" else "1", c)
            s["status"] = "off"
        elif name == "poweron":
            sh("docker", "start", c)
            wait_systemd(c)
            s["status"] = "running"
        elif name == "reboot":
            sh("docker", "restart", c)
        elif name == "change_type":
            ty = next((t for t in TYPES if t["name"] == b.get("server_type")), None)
            if not ty:
                return self.error(422, "invalid_input", "unknown server type")
            if s["status"] != "off":
                return self.error(423, "server_not_stopped", "the server must be off")
            s["server_type"] = ty
            if b.get("upgrade_disk"):
                s["primary_disk_size"] = ty["disk"]
        else:
            return self.error(404, "not_found", f"no action {name}")
        log(f"server {sid}: {name}")
        return self.reply(201, {"action": action(name)})

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def do_DELETE(self):
        self.route("DELETE")

    def do_PUT(self):
        self.route("PUT")


class Trace(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        body = (f"fl=1f1\nh=1.1.1.1\nip={self.client_address[0]}\nts={time.time():.3f}\n"
                "visit_scheme=https\nuag=e2e\ncolo=FRA\nsliver=none\nhttp=http/1.1\nloc=DE\n"
                "tls=TLSv1.3\nsni=off\nwarp=off\ngateway=off\nrbi=off\nkex=X25519\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def certs():
    os.makedirs("/certs", exist_ok=True)
    os.chdir("/certs")
    sh("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key",
       "-out", "ca.crt", "-days", "30", "-subj", "/CN=Modula e2e mock CA",
       "-addext", "basicConstraints=critical,CA:TRUE",
       "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    sh("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "leaf.key", "-out",
       "leaf.csr", "-subj", "/CN=1.1.1.1")
    with open("leaf.ext", "w") as f:
        f.write("subjectAltName=IP:1.1.1.1\nbasicConstraints=critical,CA:FALSE\n"
                "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    sh("openssl", "x509", "-req", "-in", "leaf.csr", "-CA", "ca.crt", "-CAkey", "ca.key",
       "-CAcreateserial", "-out", "leaf.crt", "-days", "30", "-extfile", "leaf.ext")


def main_():
    global MY_IP
    MY_IP = my_ip()
    certs()
    subprocess.run(["ip", "addr", "add", "1.1.1.1/32", "dev", "lo"], capture_output=True)
    trace = ThreadingHTTPServer(("0.0.0.0", 443), Trace)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain("/certs/leaf.crt", "/certs/leaf.key")
    trace.socket = ctx.wrap_socket(trace.socket, server_side=True)
    threading.Thread(target=trace.serve_forever, daemon=True).start()
    log(f"hetzner mock on http://{MY_IP}:{PORT}/v1, trace on 1.1.1.1:443")
    ThreadingHTTPServer(("0.0.0.0", PORT), Api).serve_forever()


if __name__ == "__main__":
    main_()
