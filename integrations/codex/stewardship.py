"""Quiet, owner-bound tag maintenance. No hooks, provider retries or graph copies.

The SQLite file is an auxiliary index/intent ledger, not a second memory store.
One device-local owner lease and UTC-day envelope are shared across sessions;
callers also reserve and settle their existing session provider allowance.
Token figures are conservative admission estimates/observations, not a provider
hard cap: wrapper/reasoning usage can exceed the visible authored byte envelope;
known overages halt further admission and unknown usage fences the whole epoch.
"""
from __future__ import annotations

from contextlib import contextmanager, closing
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import sqlite3
import time

from librarian_policy import resolve

ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
SHA = re.compile(r"[0-9a-f]{64}\Z")
CODEC = "mneme.routing-content.v2"
DAY = 86400
MAX_CURSOR = 4096
# A node needs this much wire room even before its summary/tags. These bounds
# derive request count from the native read envelope, not total graph size.
MIN_NODE_BYTES = 256


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode()


def enabled(config):
    return (config.get("tag_stewardship") is True
            and config.get("recording_mode") == "automatic"
            and config.get("memory_mode", config.get("recall_mode")) == "async")


def journal_root():
    # Not state_dir: two configured sessions of the same owner cannot buy two
    # daily envelopes. The database's canonical identity names this local index.
    return Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "mneme/codex-stewardship"


class Journal:
    def __init__(self, db_id, *, root=None, now=None):
        if not isinstance(db_id, str) or not ULID.fullmatch(db_id):
            raise ValueError("invalid_owner")
        self.db_id = db_id
        self.now = time.time() if now is None else now
        root = journal_root() if root is None else Path(root)
        if not root.is_absolute() or root.is_symlink():
            raise ValueError("journal_path")
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        root = root.resolve(strict=True)
        self.path = root / (db_id + ".sqlite3")
        if self.path.is_symlink() or self.path.exists() and (not self.path.is_file() or self.path.stat().st_nlink != 1):
            raise ValueError("journal_path")
        self.fd = os.open(self.path.with_suffix(".lock"), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        self.db = None
        created = False
        try:
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if self.path.exists():
                # Positive current-schema classification before any SQL write.
                self.db = sqlite3.connect(self.path.as_uri()+"?mode=rw", uri=True, timeout=0, isolation_level=None)
                self.db.row_factory = sqlite3.Row
                tables = {row[0] for row in self.db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                indexes = {row[0] for row in self.db.execute("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL")}
                if (self.db.execute("PRAGMA user_version").fetchone()[0] != 1
                        or tables != {"meta","work","usage","reservations","intents","sqlite_sequence"}
                        or indexes != {"work_dirty","work_cold","work_retry","intents_target"}
                        or self.get("owner") != db_id):
                    raise ValueError("unsupported_journal")
                # Column reads fail closed on torn or altered known schemas.
                self.db.execute("SELECT id,dirty,cold,fingerprint,guide,outcome,examined,retry_after FROM work LIMIT 0")
                self.db.execute("SELECT epoch,attempts,input,output,unknown,native_rounds FROM usage LIMIT 0")
                self.db.execute("SELECT token,epoch,targets,guide FROM reservations LIMIT 0")
                self.db.execute("SELECT seq,id,fingerprint,guide,tags,status,created FROM intents LIMIT 0")
            else:
                self.db = sqlite3.connect(self.path, timeout=0, isolation_level=None)
                created = True
                os.chmod(self.path, 0o600)
                self.db.row_factory = sqlite3.Row
                self.db.executescript("""
                    BEGIN IMMEDIATE;
                    CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                    CREATE TABLE work(id TEXT PRIMARY KEY, dirty INTEGER NOT NULL DEFAULT 0,
                        cold INTEGER NOT NULL DEFAULT 0, fingerprint TEXT, guide TEXT, outcome TEXT,
                        examined REAL NOT NULL DEFAULT 0,retry_after REAL NOT NULL DEFAULT 0);
                    CREATE INDEX work_dirty ON work(dirty DESC,examined,id);
                    CREATE INDEX work_cold ON work(cold,id);
                    CREATE INDEX work_retry ON work(retry_after,id);
                    CREATE TABLE usage(epoch INTEGER PRIMARY KEY,attempts INTEGER NOT NULL DEFAULT 0,
                        input INTEGER NOT NULL DEFAULT 0,output INTEGER NOT NULL DEFAULT 0,
                        unknown INTEGER NOT NULL DEFAULT 0,native_rounds INTEGER NOT NULL DEFAULT 0);
                    CREATE TABLE reservations(token TEXT PRIMARY KEY,epoch INTEGER NOT NULL,
                        targets TEXT NOT NULL,guide TEXT NOT NULL);
                    CREATE TABLE intents(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL,
                        fingerprint TEXT NOT NULL,guide TEXT NOT NULL,tags TEXT NOT NULL,
                        status TEXT NOT NULL,created REAL NOT NULL);
                    CREATE INDEX intents_target ON intents(id,seq);
                    PRAGMA user_version=1;
                    COMMIT;
                """)
                self.put("owner", db_id)
        except BaseException:
            if self.db is not None:
                self.db.close()
            os.close(self.fd)
            if created:
                self.path.unlink(missing_ok=True)
            raise

    def close(self):
        self.db.close()
        os.close(self.fd)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    @contextmanager
    def transaction(self):
        self.db.execute("BEGIN IMMEDIATE")
        try:
            yield
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise

    @contextmanager
    def lease(self):
        # Construction already holds the canonical adjacent exclusive lease;
        # never open a SQLite handle before owning it.
        yield

    def get(self, key):
        row = self.db.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        return row[0] if row else None

    def put(self, key, value):
        self.db.execute("INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        (key, str(value)))

    def enqueue(self, ids):
        with self.transaction():
            for identifier in ids:
                if not isinstance(identifier, str) or not ULID.fullmatch(identifier):
                    raise ValueError("invalid_node")
                self.db.execute("INSERT INTO work(id,dirty) VALUES(?,2) ON CONFLICT(id) DO UPDATE SET dirty=2", (identifier,))

    def needs_page(self):
        if self.db.execute("SELECT 1 FROM work WHERE cold=1 LIMIT 1").fetchone():
            return False
        return self.now >= float(self.get("next_sweep") or 0)

    def page(self, ids, cursor):
        if cursor is not None and (not isinstance(cursor, str) or not 0 < len(cursor.encode()) <= MAX_CURSOR):
            raise ValueError("invalid_cursor")
        with self.transaction():
            for identifier in ids:
                if not isinstance(identifier, str) or not ULID.fullmatch(identifier):
                    raise ValueError("invalid_node")
                self.db.execute("INSERT INTO work(id,cold) VALUES(?,1) ON CONFLICT(id) DO UPDATE SET cold=1", (identifier,))
            self.put("cursor", cursor or "")
            # A finished pass does not immediately start the same cold scan.
            self.put("next_sweep", self.now + DAY if cursor is None else 0)

    def candidates(self, guide, limit):
        # Due technical failures re-enter the queue without invalidating stable
        # semantic abstentions. Indexed and bounded, never an immediate retry.
        due = self.db.execute("SELECT id FROM work WHERE retry_after>0 AND retry_after<=? ORDER BY retry_after,id LIMIT ?",(self.now,limit)).fetchall()
        with self.transaction():
            for row in due:
                self.db.execute("UPDATE work SET dirty=MAX(dirty,1),retry_after=0 WHERE id=?",(row[0],))
        # Cold material gets a reserved half even under a perpetual dirty queue.
        cold = self.db.execute("SELECT id FROM work WHERE cold=1 ORDER BY id LIMIT ?", ((limit+1)//2,)).fetchall()
        # Explicit changed-node notifications outrank policy/technical rechecks.
        dirty = self.db.execute("SELECT id FROM work WHERE dirty>0 ORDER BY dirty DESC,examined,id LIMIT ?", (limit,)).fetchall()
        # Policy invalidation is a separate bounded ID walk, not a table scan
        # for every unchanged node and never a reset of the cold-store cursor.
        with self.transaction():
            if self.get("guide") != guide:
                self.put("guide",guide)
                self.put("policy_cursor","")
        cursor = self.get("policy_cursor") or ""
        policy_rows = self.db.execute("SELECT id,guide FROM work WHERE id>? ORDER BY id LIMIT ?",(cursor,limit)).fetchall()
        with self.transaction():
            for row in policy_rows:
                if row["guide"] != guide:
                    self.db.execute("UPDATE work SET dirty=MAX(dirty,1) WHERE id=?",(row["id"],))
            if policy_rows:
                self.put("policy_cursor",policy_rows[-1]["id"])
        stale = [(row["id"],) for row in policy_rows if row["guide"] != guide]
        more_cold = self.db.execute("SELECT id FROM work WHERE cold=1 ORDER BY id LIMIT ?", (limit,)).fetchall()
        result = []
        for row in (*cold, *dirty, *stale, *more_cold):
            if row[0] not in result:
                result.append(row[0])
            if len(result) == limit:
                break
        return result

    def examined(self, identifier, fingerprint, guide):
        row = self.db.execute("SELECT fingerprint,guide,outcome FROM work WHERE id=?", (identifier,)).fetchone()
        return (row is not None and tuple(row)[:2] == (fingerprint,guide)
                and row["outcome"] in ("noop","unresolved","ineligible","applied","unchanged"))

    def deferred(self, identifier, fingerprint, guide):
        row=self.db.execute("SELECT fingerprint,guide,retry_after FROM work WHERE id=?",(identifier,)).fetchone()
        return (row is not None and tuple(row)[:2]==(fingerprint,guide)
                and row["retry_after"]>self.now)

    def mark(self, identifier, fingerprint, guide, outcome):
        self.db.execute("""INSERT INTO work(id,fingerprint,guide,outcome,examined) VALUES(?,?,?,?,?)
            ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,guide=excluded.guide,
            outcome=excluded.outcome,examined=excluded.examined,dirty=0,cold=0,retry_after=0""",
            (identifier,fingerprint,guide,outcome,self.now))

    def defer(self, identifier, fingerprint, guide, reason):
        self.mark(identifier,fingerprint,guide,reason)
        self.db.execute("UPDATE work SET retry_after=? WHERE id=?",((int(self.now//DAY)+1)*DAY,identifier))

    def recover(self, budget):
        # Process death after durable reservation means unknown spend. Never
        # infer a successful retag from matching tags, or replay the old intent.
        with self.transaction():
            for row in self.db.execute("SELECT * FROM reservations").fetchall():
                self.db.execute("UPDATE usage SET unknown=1,input=MAX(input,?),output=MAX(output,?) WHERE epoch=?",
                                (budget.input_tokens,budget.output_tokens,row["epoch"]))
                for target in json.loads(row["targets"]):
                    self.defer(target["id"],target["content_fingerprint"],row["guide"],"usage_unknown")
            self.db.execute("DELETE FROM reservations")
            for row in self.db.execute("SELECT id,fingerprint,guide FROM intents WHERE status='intent'").fetchall():
                self.defer(row["id"],row["fingerprint"],row["guide"],"write_unknown")
            self.db.execute("UPDATE intents SET status='unknown' WHERE status='intent'")

    def reserve_native(self, budget):
        epoch = int(self.now // DAY)
        with self.transaction():
            self.db.execute("INSERT OR IGNORE INTO usage(epoch) VALUES(?)",(epoch,))
            row = self.db.execute("SELECT native_rounds FROM usage WHERE epoch=?",(epoch,)).fetchone()
            if (row[0] >= budget.attempts or self.now < float(self.get("next_batch") or 0)):
                return False
            self.db.execute("UPDATE usage SET native_rounds=native_rounds+1 WHERE epoch=?",(epoch,))
            self.put("next_batch",self.now+budget.native_seconds)
        return True

    def reserve(self, targets, guide, budget):
        epoch = int(self.now // DAY)
        token = secrets.token_hex(16)
        with self.transaction():
            self.db.execute("INSERT OR IGNORE INTO usage(epoch) VALUES(?)",(epoch,))
            usage = self.db.execute("SELECT * FROM usage WHERE epoch=?",(epoch,)).fetchone()
            if (usage["unknown"] or usage["attempts"] >= budget.attempts
                    or usage["input"] + budget.stewardship_prompt_bytes > budget.input_tokens - budget.selector_room_input_tokens
                    or usage["output"] + budget.stewardship_answer_bytes > budget.output_tokens - budget.selector_room_output_tokens):
                return None
            self.db.execute("UPDATE usage SET attempts=attempts+1 WHERE epoch=?",(epoch,))
            # Only finite identity/fingerprint projections survive, not prompts.
            evidence = [{k:t[k] for k in ("id","content_fingerprint")} for t in targets]
            self.db.execute("INSERT INTO reservations VALUES(?,?,?,?)",(token,epoch,encoded(evidence).decode(),guide))
        return token

    def settle(self, token, result, budget):
        row = self.db.execute("SELECT * FROM reservations WHERE token=?",(token,)).fetchone()
        if row is None:
            return False
        usage = result.get("usage")
        known = (isinstance(usage,dict) and all(type(usage.get(k)) is int and usage[k]>=0 for k in ("input_tokens","output_tokens")))
        attempted = result.get("provider_attempt") is True
        with self.transaction():
            if attempted and not known:
                self.db.execute("UPDATE usage SET unknown=1,input=MAX(input,?),output=MAX(output,?) WHERE epoch=?",
                                (budget.input_tokens,budget.output_tokens,row["epoch"]))
            elif attempted:
                self.db.execute("UPDATE usage SET input=input+?,output=output+? WHERE epoch=?",(usage["input_tokens"],usage["output_tokens"],row["epoch"]))
            else:
                self.db.execute("UPDATE usage SET attempts=MAX(0,attempts-1) WHERE epoch=?",(row["epoch"],))
            self.db.execute("DELETE FROM reservations WHERE token=?",(token,))
        return not attempted or known

    def intent(self, target, guide, tags):
        with self.transaction():
            cur = self.db.execute("INSERT INTO intents(id,fingerprint,guide,tags,status,created) VALUES(?,?,?,?,?,?)",
                (target["id"],target["content_fingerprint"],guide,encoded(tags).decode(),"intent",self.now))
            # Record attempt before the write. Crash recovery leaves history
            # unknown; later bounded fresh-state classification may reassess it.
            self.mark(target["id"],target["content_fingerprint"],guide,"intent")
        return cur.lastrowid

    def acknowledge(self, seq, status):
        if status not in ("applied","unchanged","refused","unknown"):
            raise ValueError("invalid_acknowledgement")
        self.db.execute("UPDATE intents SET status=? WHERE seq=? AND status='intent'",(status,seq))


def enqueue(config, db_id, ids):
    """Best-effort changed-node notice; caller supplies a verified owner identity."""
    if not enabled(config):
        return False
    try:
        if not isinstance(ids,(list,tuple)) or len(encoded(ids)) > resolve(config).native_read_bytes:
            return False
        with Journal(db_id) as journal:
            journal.enqueue(ids)
        return True
    except (OSError,ValueError,TypeError,sqlite3.Error):
        return False


class NativeOwner:
    """Bound one existing owner and aggregate decoded bytes/deadline per batch."""
    def __init__(self, config, budget):
        from target_policy import policy_for, target_kwargs
        from hook_recall import _config_snapshot
        self.config = config
        self.policy, service = policy_for(config["service_config"],config["project_root"],**target_kwargs(config))
        self.snapshot = _config_snapshot(Path(config["service_config"]))
        self.deadline = time.monotonic() + budget.native_seconds
        self.remaining_bytes = budget.native_read_bytes
        self.summary_bytes = min(16 * 1024,budget.native_read_bytes)
        from mcp_client import McpClient
        token = os.environ.get(service.token_env) if service.token_env else None
        if service.token_env and not token:
            raise ValueError("owner_unavailable")
        self.client = McpClient(service.url, token=token, timeout=min(30,budget.native_seconds))
        self.db_id = None

    def __enter__(self):
        try:
            self.client.connect()
            self.db_id = self.policy.catalog_identity(self.call("databases",{}))
            return self
        except BaseException:
            self.__exit__(None,None,None)
            raise

    def __exit__(self, *_):
        self.client.timeout = min(.1,max(.001,self.deadline-time.monotonic()))
        self.client.close()

    def authorize(self):
        # Native client validated the catalog during connect; never decode it
        # again or infer support from remote initialize fields/old owners.
        capabilities=self.charge(self.client.owner_capabilities())
        if not all(capabilities.get(key) is True for key in ("retag_content_guards","tag_vocabulary")):
            raise ValueError("strong_retag_unavailable")

    def _admit(self):
        from hook_recall import _config_snapshot
        if _config_snapshot(Path(self.config["service_config"])) != self.snapshot:
            raise ValueError("owner_config_changed")
        self.policy.validate_workspace()
        remaining = self.deadline-time.monotonic()
        if remaining<=0 or self.remaining_bytes<=0:
            raise TimeoutError("native_budget")
        self.client.timeout = min(30,remaining)

    def charge(self, value):
        self.remaining_bytes -= len(encoded(value))
        if self.remaining_bytes < 0:
            raise ValueError("native_byte_budget")
        return value


    def call(self, name, args):
        self._admit()
        if name != "databases":
            args = {"db":self.policy.db_alias,"expected_db_id":self.db_id,**args}
        return self.charge(self.client.call_tool(name,args))

    def page(self, cursor, limit):
        result = self.call("list",{"kind":"nodes","status":"active","limit":limit,**({"after":cursor} if cursor else {})})
        if (not isinstance(result,dict) or result.get("db_id")!=self.db_id or result.get("kind")!="nodes"
                or not isinstance(result.get("items"),list) or len(result["items"])>limit
                or type(result.get("has_more")) is not bool
                or result["has_more"] != (result.get("next_cursor") is not None)):
            raise ValueError("invalid_list")
        if result.get("next_cursor") is not None and result["next_cursor"] == cursor:
            raise ValueError("nonprogressing_list")
        ids=[]
        for item in result["items"]:
            identifier=item.get("id") if isinstance(item,dict) else None
            if not isinstance(identifier,str) or not ULID.fullmatch(identifier) or identifier in ids:
                raise ValueError("invalid_list_identity")
            ids.append(identifier)
        return ids,result.get("next_cursor")

    def target(self, identifier):
        value = self.call("get",{"id":identifier,"body":False})
        if (not isinstance(value,dict) or value.get("db_id")!=self.db_id or value.get("id")!=identifier
                or value.get("content_fingerprint_codec")!=CODEC
                or not isinstance(value.get("content_fingerprint"),str)
                or not SHA.fullmatch(value["content_fingerprint"])):
            raise ValueError("invalid_target_identity")
        fingerprint=value["content_fingerprint"]
        if value.get("status")!="active" or value.get("memory_kind")!={"kind":"semantic"}:
            return None,fingerprint
        summary,tags=value.get("summary"),value.get("tags")
        if (not isinstance(summary,str) or not 0<len(summary.encode())<=self.summary_bytes
                or value.get("summary_truncated",False) is not False
                or not isinstance(tags,list) or len(tags)>64
                or any(not isinstance(t,str) or not 0<len(t.encode())<=256 for t in tags)
                or len(set(tags))!=len(tags)):
            raise ValueError("invalid_target")
        return {"id":identifier,"summary":summary,"tags":tags,"content_fingerprint":fingerprint},fingerprint

    def retag(self, target, tags, guards):
        result = self.call("retag",{"id":target["id"],"expected_tags":target["tags"],"tags":tags,
            "expected_content_fingerprint":target["content_fingerprint"],"guard_nodes":guards})
        return result


def step(config, session_id, runtime, reserve, account, *, allowed=lambda:True):
    """One finite idle batch. Failures are private journal state, never hook text.

    reserve(key)/account(key,result) share the existing foreground session ledger.
    The model sees no tools; only checked native retag is a mutation authority.
    """
    if not enabled(config) or not allowed():
        return False
    try:
        budget=resolve(config)
        with NativeOwner(config,budget) as owner, Journal(owner.db_id) as journal, journal.lease():
            journal.recover(budget)
            if not journal.reserve_native(budget):
                return False
            try:
                owner.authorize()
            except Exception:
                journal.put("last_outcome","owner_capability_unavailable")
                return False
            import tag_context
            import stewardship_contract
            # Keep actual native byte room for bounded policy/vocabulary after
            # reading a finite candidate pool. That permits relevance-directed
            # prefix lookup instead of always consulting the alphabetical head.
            context_room=min(tag_context.MAX_CONTEXT_BYTES,owner.remaining_bytes//2)
            max_nodes=min(64,max(1,(owner.remaining_bytes-context_room)//MIN_NODE_BYTES))
            if journal.needs_page():
                ids,cursor=owner.page(journal.get("cursor") or None,max_nodes)
                journal.page(ids,cursor)
            previous_guide=journal.get("guide") or ""
            pool=[]
            for identifier in journal.candidates(previous_guide,max_nodes):
                if not allowed() or time.monotonic()>=owner.deadline or owner.remaining_bytes-context_room<MIN_NODE_BYTES:
                    break
                try:
                    target,fingerprint=owner.target(identifier)
                except Exception:
                    journal.defer(identifier,None,previous_guide,"read_deferred")
                    continue
                if target is None or identifier==config.get("tag_guide_id"):
                    journal.mark(identifier,fingerprint,previous_guide,"ineligible")
                    continue
                pool.append(target)
            context=tag_context.collect(config,expected_db_id=owner.db_id,
                timeout=max(.001,owner.deadline-time.monotonic()),max_bytes=max(1,min(context_room,owner.remaining_bytes)),
                cue=" ".join(t["summary"] for t in pool)[:budget.stewardship_prompt_bytes],
                seed_tags=tuple(dict.fromkeys(tag for t in pool for tag in t["tags"])))
            owner.remaining_bytes -= context.decoded_bytes
            if not context.enabled or context.db_id != owner.db_id or owner.remaining_bytes<=0 or not allowed():
                journal.put("last_outcome","context_unavailable_or_interrupted")
                return False
            semantic_guide=context.snapshot()["guide_revision_sha256"]
            # Explicit classifier/envelope revisions can change feasibility and
            # judgment; vocabulary growth alone cannot invalidate examination.
            guide=hashlib.sha256(encoded({"guide":semantic_guide,"contract":stewardship_contract.REVISION,
                "effort":budget.effort})).hexdigest()
            journal.put("guide_revision",semantic_guide)
            journal.candidates(guide,max_nodes)  # bounded policy invalidation, separate from cold cursor
            targets=[]
            for target in pool:
                identifier,fingerprint=target["id"],target["content_fingerprint"]
                if journal.examined(identifier,fingerprint,guide) or journal.deferred(identifier,fingerprint,guide):
                    journal.db.execute("UPDATE work SET dirty=0,cold=0 WHERE id=?",(identifier,))
                    continue
                prompt,_=stewardship_contract.prepare([*targets,target],context,budget=budget)
                if prompt is None:
                    if not targets:
                        journal.defer(identifier,fingerprint,guide,"resource_deferred")
                        continue
                    break
                targets.append(target)
            if not targets or not allowed():
                journal.put("last_outcome","scan_only_or_interrupted")
                return False
            token=journal.reserve(targets,guide,budget)
            if token is None:
                journal.put("last_outcome","owner_provider_budget")
                return False
            key="stewardship:"+token
            if not reserve(key):
                journal.settle(token,{"provider_attempt":False},budget)
                journal.put("last_outcome","foreground_or_session_budget")
                return False
            try:
                result=runtime.steward(targets,context,timeout=min(45,budget.native_seconds))
            except Exception:
                result={"provider_attempt":True,"usage":None,"decisions":None,"reason":"provider_error"}
            session_settled=account(key,result)
            owner_settled=journal.settle(token,result,budget)
            decisions=result.get("decisions")
            # Validate again at authority boundary: injected or replaced runtimes
            # cannot supply unchecked mutations.
            _,validation=stewardship_contract.prepare(targets,context,budget=budget)
            try:
                decisions=stewardship_contract.validate_answer({"decisions":decisions},validation)["decisions"]
            except (ValueError,TypeError,KeyError):
                decisions=None
            if not session_settled or not owner_settled or decisions is None:
                for target in targets:
                    journal.defer(target["id"],target["content_fingerprint"],guide,"assessment_deferred")
                journal.put("last_outcome","assessment_deferred")
                return True
            # The read deadline excludes the model turn, but does not mint an
            # unbounded retry period. One fresh bounded write window, no retry.
            owner.deadline=time.monotonic()+budget.native_seconds
            lookup={target["id"]:target for target in targets}
            for decision in decisions:
                if not allowed():
                    break
                target=lookup[decision["id"]]
                disposition=decision["disposition"]
                if disposition!="retag":
                    journal.mark(target["id"],target["content_fingerprint"],guide,disposition)
                    continue
                seq=journal.intent(target,guide,decision["tags"])
                try:
                    receipt=owner.retag(target,decision["tags"],context.content_guards)
                    status=_acknowledgement(receipt,owner.db_id,target,decision["tags"])
                except Exception as error:
                    from mcp_client import McpToolError
                    status="refused" if isinstance(error,McpToolError) else "unknown"
                journal.acknowledge(seq,status)
                if status in ("refused","unknown"):
                    journal.defer(target["id"],target["content_fingerprint"],guide,
                                  "write_deferred" if status=="refused" else "write_unknown")
                else:
                    journal.mark(target["id"],target["content_fingerprint"],guide,status)
            journal.put("last_outcome","batch_examined")
            return True
    except Exception:
        # Hook path remains quiet. Outstanding durable intents/reservations are
        # conservatively recovered when the owner lease next becomes available.
        return False


def _acknowledgement(receipt, db_id, target, tags):
    """Only a checked native acknowledgement establishes historical success."""
    if not isinstance(receipt,dict) or receipt.get("db_id")!=db_id or receipt.get("id")!=target["id"]:
        return "unknown"
    if (type(receipt.get("changed")) is bool and isinstance(receipt.get("tags"),list)
            and len(receipt["tags"])==len(tags) and set(receipt["tags"])==set(tags)
            and receipt["changed"] == (set(target["tags"]) != set(tags))):
        return "applied" if receipt["changed"] else "unchanged"
    return "unknown"


def inspect(db_id, *, limit=16, root=None):
    """Read-only local journal status; absence never creates directories/files."""
    if (not isinstance(db_id,str) or not ULID.fullmatch(db_id)
            or type(limit) is not int or not 1<=limit<=64):
        raise ValueError("invalid_inspection")
    base=journal_root() if root is None else Path(root)
    path=base/(db_id+".sqlite3")
    if not path.exists():
        return {"db_id":db_id,"outcome":"absent"}
    if path.is_symlink() or not path.is_file() or path.stat().st_nlink!=1:
        raise ValueError("journal_path")
    with closing(sqlite3.connect(path.resolve().as_uri()+"?mode=ro",uri=True,timeout=0)) as db:
        db.row_factory=sqlite3.Row
        db.execute("BEGIN")  # Read-only SQLite snapshot; never a second writer.
        if db.execute("PRAGMA user_version").fetchone()[0]!=1:
            raise ValueError("unsupported_journal")
        owner=db.execute("SELECT value FROM meta WHERE key='owner'").fetchone()
        if owner is None or owner[0]!=db_id:
            raise ValueError("journal_owner")
        meta={row["key"]:row["value"] for row in db.execute(
            "SELECT key,value FROM meta WHERE key IN ('cursor','next_sweep','guide','guide_revision','policy_cursor','next_batch','last_outcome')")}
        sample=db.execute("SELECT outcome FROM work ORDER BY id LIMIT ?",(limit+1,)).fetchall()
        complete=len(sample)<=limit
        counts={}
        for row in sample[:limit]:
            name=row[0] or "pending"
            counts[name]=counts.get(name,0)+1
        counts={name:{"status":"exact" if complete else "lower_bound","value":amount} for name,amount in counts.items()}
        latest=db.execute("SELECT * FROM usage ORDER BY epoch DESC LIMIT 1").fetchone()
        actions=[dict(row) for row in db.execute("SELECT seq,id,fingerprint,guide,status,created FROM intents ORDER BY seq DESC LIMIT ?",(limit,))]
        return {"db_id":db_id,"outcome":"available","counts":counts,"progress":meta,
                "count_coverage":{"complete":complete,"examined":min(limit,len(sample))},
                "usage":dict(latest) if latest else None,"actions":actions}


def main(argv=None):
    import argparse
    parser=argparse.ArgumentParser(description="Read-only bounded local tag-stewardship journal status")
    parser.add_argument("--db-id",required=True)
    parser.add_argument("--limit",type=int,default=16)
    args=parser.parse_args(argv)
    try:
        print(encoded(inspect(args.db_id,limit=args.limit)).decode())
        return 0
    except (OSError,ValueError,sqlite3.Error):
        print('{"outcome":"unavailable"}')
        return 1


if __name__=="__main__":
    raise SystemExit(main())
