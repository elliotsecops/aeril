#!/usr/bin/python3
# Hermes fake para tests P2-S1 / R1 — controlado por archivos dentro de
# HERMES_HOME (el entorno sanitizado siempre lo expone). Sin credenciales.
#
# Modos (archivo `fake-init-mode`, legacy `fake-mode`):
#   valid | notif-masquerade | string-result | wrong-id | null-result | error
#   | bare-result | missing-protocol | oversized | eof-mid-frame
#   | no-response | endless-notifications | notifications-then-valid
#   | hang | ignore-term | slow:<segundos> | linger
# Auth (`fake-auth-mode`): positive | positive-exit1 | not-currently |
#   not-signed | logged-out | no-credentials | other-provider |
#   contradictory | malformed | empty
# Sin `fake-auth-mode`: positive si existe `fake-auth`, si no logged-out.
import json, os, signal, sys, time

args = sys.argv[1:]
home = os.environ.get("HERMES_HOME") or ""


def proc_start(pid):
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            tail = f.read().rsplit(b")", 1)[1].split()
        return int(tail[19])
    except (OSError, IndexError, ValueError):
        return 0


def read_opt(name, default=""):
    if not home:
        return default
    try:
        with open(os.path.join(home, name)) as f:
            return f.read().strip()
    except OSError:
        return default


if args and args[0] == "--version":
    print("hermes 0.0.0-fake")
    sys.exit(0)

if len(args) >= 2 and args[0] == "acp" and args[1] == "--version":
    print("hermes 0.0.0-fake")
    sys.exit(0)

if args and args[0] == "auth":
    mode = read_opt("fake-auth-mode", "")
    if mode.startswith("slow:"):
        try:
            with open(os.path.join(home, "fake-auth-started"), "w") as f:
                f.write("1")
        except OSError:
            pass
        time.sleep(float(mode.split(":", 1)[1]))
        mode = "positive"
    if not mode:
        mode = "positive" if os.path.exists(os.path.join(home, "fake-auth")) else "logged-out"
    if mode == "positive":
        print("nous: logged in")
        sys.exit(0)
    if mode == "truncated-positive":
        # The positive status is inside the retained prefix; the later
        # contradiction must still make the probe fail closed.
        sys.stdout.write("nous: logged in\n")
        sys.stdout.write("x" * 70000 + "\n")
        sys.stdout.write("nous: logged out\n")
        sys.stdout.flush()
        sys.exit(0)
    if mode == "positive-flood":
        sys.stdout.write("nous: logged in\n")
        sys.stdout.write("x" * 200000 + "\n")
        sys.stdout.flush()
        sys.exit(0)
    if mode == "stderr-contradictory":
        print("nous: logged in")
        sys.stderr.write("nous: logged out\n")
        sys.stderr.flush()
        sys.exit(0)
    if mode == "stderr-flood":
        print("nous: logged in")
        sys.stderr.write("x" * 200000 + "\n")
        sys.stderr.flush()
        sys.exit(0)
    if mode == "descendant-stdout":
        print("nous: logged in")
        sys.stdout.flush()
        pid = os.fork()
        if pid == 0:
            time.sleep(600)
            os._exit(0)
        try:
            with open(os.path.join(home, "fake-descendant-pid"), "w") as f:
                f.write(f"{pid} {proc_start(pid)}\n")
        except OSError:
            pass
        sys.exit(0)
    if mode == "timeout-descendants":
        pids = []
        for _ in range(2):
            pid = os.fork()
            if pid == 0:
                time.sleep(600)
                os._exit(0)
            pids.append(pid)
        try:
            with open(os.path.join(home, "fake-descendant-pid"), "w") as f:
                f.write(" ".join(f"{p} {proc_start(p)}" for p in pids) + "\n")
        except OSError:
            pass
        time.sleep(600)
        sys.exit(0)
    if mode == "positive-exit1":
        print("nous: logged in")
        sys.exit(1)
    if mode == "not-currently":
        print("nous: not currently logged in")
        sys.exit(0)
    if mode == "not-signed":
        print("nous: not signed in")
        sys.exit(0)
    if mode == "logged-out":
        print("nous: logged out")
        sys.exit(0)
    if mode == "no-credentials":
        print("nous: no credentials")
        sys.exit(0)
    if mode == "other-provider":
        print("github: logged in")
        sys.exit(0)
    if mode == "contradictory":
        print("nous: logged in")
        print("nous: logged out")
        sys.exit(0)
    if mode == "malformed":
        print(";;; not json")
        sys.exit(0)
    if mode == "empty":
        sys.exit(0)
    print("nous: logged out")
    sys.exit(0)

if args and args[0] == "acp":
    dump = os.environ.get("FAKE_DUMP") or ""
    if home:
        dump = os.path.join(home, "fake-env.json")
    if dump:
        keys = ["HERMES_HOME", "HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
                "XDG_DATA_HOME", "XDG_STATE_HOME", "GH_CONFIG_DIR", "PATH"]
        data = {k: os.environ.get(k, "") for k in keys}
        try:
            with open(dump, "w") as f:
                json.dump(data, f)
        except OSError:
            pass
    if home:
        try:
            with open(os.path.join(home, "fake-pid"), "w") as f:
                f.write(str(os.getpid()))
            with open(os.path.join(home, "fake-pid-start"), "w") as f:
                f.write(str(proc_start(os.getpid())))
        except OSError:
            pass

    mode = read_opt("fake-init-mode", "") or read_opt("fake-mode", "") or "valid"
    if mode == "full":
        mode = "valid"
    if mode.startswith("slow:"):
        time.sleep(float(mode.split(":", 1)[1]))
        mode = "valid"
    if mode == "ignore-term":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(600)
        sys.exit(0)
    if mode == "hang":
        time.sleep(600)
        sys.exit(0)
    if mode == "oversized":
        sys.stdout.write('{"jsonrpc":"2.0","id":1,"result":{"blob":"' + "x" * 1100000 + '"}}\n')
        sys.stdout.flush()
        time.sleep(600)
        sys.exit(0)
    if mode == "eof-mid-frame":
        sys.stdout.write('{"jsonrpc":"2.0","id":1,"resu')
        sys.stdout.flush()
        sys.exit(0)
    if mode == "endless-notifications":
        for i in range(100000):
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "method": "note", "params": {"i": i}}) + "\n")
            sys.stdout.flush()
        time.sleep(600)
        sys.exit(0)
    if mode == "notifications-then-valid":
        print(json.dumps({"jsonrpc": "2.0", "method": "note", "params": {"a": 1}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "note", "params": {"b": 2}}), flush=True)

    def respond_init():
        if mode == "notif-masquerade":
            return {"jsonrpc": "2.0", "method": "note", "result": {"fake": True}}
        if mode == "string-result":
            return {"jsonrpc": "2.0", "id": 1, "result": "ok"}
        if mode == "wrong-id":
            return {"jsonrpc": "2.0", "id": 99, "result": {"protocolVersion": 1,
                    "agentInfo": {"name": "fake", "version": "0"}, "agentCapabilities": {},
                    "authMethods": [{"id": "terminal", "name": "setup"}]}}
        if mode == "null-result":
            return {"jsonrpc": "2.0", "id": 1, "result": None}
        if mode == "error":
            return {"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "boom"}}
        if mode == "bare-result":
            return {"result": {"protocolVersion": 1}}
        if mode == "missing-protocol":
            return {"jsonrpc": "2.0", "id": 1, "result": {"agentCapabilities": {},
                    "authMethods": [{"id": "terminal", "name": "setup"}]}}
        if mode == "missing-capabilities":
            return {"jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": 1,
                "agentInfo": {"name": "fake-hermes", "version": "0.0.0-fake"},
                "authMethods": [{"id": "terminal", "name": "setup"}]}}
        return {"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1,
                "agentInfo": {"name": "fake-hermes", "version": "0.0.0-fake"},
                "agentCapabilities": {"loadSession": True},
                "authMethods": [{"id": "terminal", "name": "setup"}]}}

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        mid = msg.get("id")
        method = msg.get("method")
        if method == "initialize":
            if mode == "no-response":
                continue
            if mode == "fragmented":
                full = json.dumps(respond_init())
                half = len(full) // 2
                sys.stdout.write(full[:half])
                sys.stdout.flush()
                time.sleep(0.2)
                sys.stdout.write(full[half:] + "\n")
                sys.stdout.flush()
                continue
            if mode == "escape-grandchild":
                read_fd, write_fd = os.pipe()
                pid = os.fork()
                if pid == 0:
                    os.close(read_fd)
                    os.setsid()
                    os.write(write_fd, b"1")
                    os.close(write_fd)
                    time.sleep(600)
                    os._exit(0)
                os.close(write_fd)
                try:
                    os.read(read_fd, 1)
                finally:
                    os.close(read_fd)
                try:
                    with open(os.path.join(home, "fake-escaped-pid"), "w") as f:
                        f.write(f"{pid} {proc_start(pid)}\n")
                except OSError:
                    pass
            print(json.dumps(respond_init()), flush=True)
        elif method == "session/new":
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {
                "sessionId": "fake-sess",
                "models": {"availableModels": [], "currentModelId": "fake"},
                "modes": {"currentModeId": "default", "availableModes": []}}}), flush=True)
        elif method == "session/prompt":
            if mode == "linger":
                if home:
                    try:
                        with open(os.path.join(home, "fake-lingering"), "w") as f:
                            f.write("1")
                    except OSError:
                        pass
                time.sleep(600)
                continue
            print(json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {
                "update": {"sessionUpdate": "agent_message_chunk",
                           "content": {"type": "text", "text": "hecho (fake)"},
                           "messageId": "m1"}}}), flush=True)
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}}), flush=True)
        elif mid is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {}}), flush=True)
    sys.exit(0)

sys.exit(0)
