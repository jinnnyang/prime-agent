from __future__ import annotations

import contextlib
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from importlib import import_module
from pathlib import Path
from unittest.mock import patch

from rlm import harness as package_harness
from rlm import rlm as callable_rlm
from rlm.harness import HarnessState, get_harness_state

# The real module: the package rebinds `rlm.harness` to the kernel facade.
harness_module = import_module("rlm.harness")

# The agent-dir override setUpModule installs: an isolated dir whose
# settings.json carries the factory opt-in setting.
_factory_gate_dir: tempfile.TemporaryDirectory | None = None
_previous_agent_dir: str | None = None


def setUpModule() -> None:
    # The factory opt-in gate refuses every factory write while the
    # `factory.enabled` setting is off (the default); the harness CRUD tests
    # exercise the write paths with the setting on, written in the real
    # settings.json shape the daemon writes. The gate's own refusal pins
    # live in test_factory.py.
    global _factory_gate_dir, _previous_agent_dir
    _factory_gate_dir = tempfile.TemporaryDirectory(prefix="harness_test_agent_dir_")
    settings = Path(_factory_gate_dir.name) / "settings.json"
    settings.write_text('{"factory": {"enabled": true}}', encoding="utf-8")
    _previous_agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
    os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = _factory_gate_dir.name


def tearDownModule() -> None:
    global _factory_gate_dir
    if _previous_agent_dir is None:
        os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
    else:
        os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = _previous_agent_dir
    if _factory_gate_dir is not None:
        _factory_gate_dir.cleanup()
        _factory_gate_dir = None

PYTHON_REFERENCE = {
    "type": "python",
    "import": "agent_skills.example",
    "callable": "run",
    "call_pattern": "await run(...)",
}

FACTORY_DAG = {
    "run": {"failure_policy": "continue", "max_parallel": 2},
    "nodes": [
        {
            "id": "collect",
            "subagent": "researcher",
            "outputs": [{"name": "findings", "type": "text"}],
        },
        {
            "id": "review",
            "subagent": {"prompt": "Review the findings."},
            "depends_on": ["collect"],
            "inputs": [{"name": "draft", "type": "text", "from": "collect.findings"}],
        },
    ],
}


@contextlib.contextmanager
def page_store():
    """A page store no other test in this module writes to.

    Memory is page-backed and the pages are files, so a test that asserts how many
    pages exist, or which page ranks first, needs a store of its own: the module
    shares one agent directory. Patching the agent dir keeps the real one untouched.
    """
    with tempfile.TemporaryDirectory(prefix="harness_pages_") as agent_dir, patch.dict(
        os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": agent_dir}
    ):
        yield Path(agent_dir)


# A state file written before memory became page-backed: `entries.memory` is its store.
LEGACY_MEMORY_STATE = {
    "schema": 1,
    "entries": {
        "memory": {
            "legacy_note": {
                "id": "legacy_note",
                "kind": "memory",
                "title": "Legacy note",
                "content": "Carried over.",
                "path": "general",
                "scope": "global",
                "metadata": {"type": "world", "hook": "read when routed"},
                "source": "agent",
                "created_at": "2026-01-01T00:00:00+00:00",
                "updated_at": "2026-01-01T00:00:00+00:00",
                "version": 3,
            }
        },
        "prompt": {},
    },
    "refinements": [],
}


class HarnessStateTest(unittest.TestCase):
    def test_crud_for_all_entry_kinds(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            # Memory is page-backed, so it has no entry kind here: the page
            # lifecycle lives in HarnessMemoryPagesTest below.

            created = {
                "prompt": state.create_prompt_note(
                    "Prompt note",
                    "Prompt content",
                    id="prompt_entry",
                    path="prompt/path",
                    metadata={"kind": "prompt"},
                ),
                "skill": state.create_skill(
                    "Skill",
                    "Skill content",
                    id="skill_entry",
                    path="skill/path",
                    reference=PYTHON_REFERENCE,
                    arguments={"target": {"type": "string", "required": True}},
                    metadata={"kind": "skill"},
                ),
                "subagent": state.create_subagent(
                    "Subagent",
                    "Subagent content",
                    id="subagent_entry",
                    path="subagent/path",
                    metadata={"kind": "subagent"},
                ),
                "factory": state.create_factory(
                    "Factory",
                    "Factory content",
                    id="factory_entry",
                    path="factory/path",
                    dag=FACTORY_DAG,
                    metadata={"kind": "factory"},
                ),
            }

            for kind, entry in created.items():
                self.assertEqual(entry.kind, kind)
                self.assertIn("content", state.get(kind, entry.id).content.lower())
                self.assertIn(entry, state.list(kind))

            state.update_prompt_note("prompt_entry", "Prompt note", "Prompt content updated")
            state.update_skill(
                "skill_entry",
                "Skill",
                "Skill content updated",
                reference=PYTHON_REFERENCE,
                arguments={"target": {"type": "string", "required": True}, "mode": {"type": "string"}},
            )
            state.update_subagent("subagent_entry", "Subagent", "Subagent content updated")
            state.update_factory("factory_entry", "Factory", "Factory content updated", dag=FACTORY_DAG)

            for kind in ("prompt", "skill", "subagent", "factory"):
                entry_id = f"{kind}_entry"
                self.assertEqual(state.get(kind, entry_id).version, 2)
                self.assertIn("updated", state.get(kind, entry_id).content)
                delete_method = getattr(state, f"delete_{'prompt_note' if kind == 'prompt' else kind}")
                self.assertTrue(delete_method(entry_id))
                self.assertIsNone(state.get(kind, entry_id))
                self.assertFalse(delete_method(entry_id))

            for kind in ("prompt", "skill", "subagent", "factory"):
                self.assertEqual(state.list(kind), [])

    def test_persists_entries_and_refinements(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            memory = state.create_memory(
                "Prefer focused patches",
                "Small harness updates are easier to validate than broad rewrites.",
                path="engineering",
            )
            skill = state.create_skill(
                "Check failures first",
                "Inspect current failure evidence before editing code.",
                id="failure_first",
                reference=PYTHON_REFERENCE,
                arguments={"failure_log": {"type": "string", "description": "Current failure evidence."}},
            )
            subagent = state.create_subagent(
                "Reviewer",
                "Review the proposed patch for regressions and missing tests.",
                metadata={"max_turns": 3},
            )
            state.create_prompt_note("Refinement cadence", "Refine only after repeated evidence.")
            event = state.record_refinement(
                "skill failed twice",
                ["updated failure_first skill", "added reviewer subagent"],
                evidence="two failed validations",
                outcome="next validation passed",
            )

            factory = state.create_factory(
                "PR review sweep",
                "Sweep review across changed files.",
                id="pr_sweep",
                dag=FACTORY_DAG,
            )

            reloaded = HarnessState(state.file_path)

            self.assertEqual(reloaded.get("memory", memory.id).content, memory.content)
            self.assertEqual(reloaded.get("skill", skill.id).version, 1)
            self.assertEqual(reloaded.get("skill", skill.id).arguments["failure_log"]["type"], "string")
            self.assertEqual(reloaded.get("subagent", subagent.id).metadata["max_turns"], 3)
            self.assertEqual(reloaded.get("factory", factory.id).arguments["dag"], FACTORY_DAG)
            self.assertEqual(reloaded.refinements[0].id, event.id)
            self.assertIn("Prefer focused patches", reloaded.overview())
            self.assertIn("factory", reloaded.overview())
            self.assertIn("await rlm.factory.run", reloaded.overview())
            self.assertIn("await rlm.factory.status(run_id)", reloaded.overview())
            self.assertIn("await rlm.factory.stop(run_id)", reloaded.overview())
            self.assertIn("await rlm.factory.resume(run_id)", reloaded.overview())
            self.assertIn("create_factory/update_factory/delete_factory", reloaded.overview())
            self.assertIn(
                "Call contract: installed Python skills use await <skill_import>(...)",
                reloaded.overview(),
            )
            overview = reloaded.overview()
            self.assertIn("handle = await rlm.spawn('sub-task', name='worker')", overview)
            self.assertIn("never the child's answer", overview)
            self.assertIn("receiver_role='parent'", overview)
            self.assertIn("await rlm.list_subagents()", overview)
            self.assertIn("receiver_role='child'", overview)
            self.assertIn("refinements: 1", reloaded.overview())

    def test_create_factory_with_invalid_dag_raises_and_does_not_store(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            invalid_dags = [
                None,
                "not a dag",
                {"nodes": []},
                {"nodes": [{"id": "a", "subagent": "w"}, {"id": "a", "subagent": "w"}]},
                {"nodes": [{"id": "a", "subagent": "w", "depends_on": ["a"]}]},
                {
                    "nodes": [
                        {"id": "a", "subagent": "w", "outputs": [{"name": "o", "type": "text"}]},
                        {"id": "b", "subagent": "w", "inputs": [{"name": "i", "type": "json", "from": "a.o"}]},
                    ]
                },
            ]
            for index, dag in enumerate(invalid_dags):
                with self.assertRaises(ValueError, msg=f"dag #{index}") as ctx:
                    state.create_factory("Broken sweep", "Should never store.", id=f"broken_{index}", dag=dag)
                self.assertTrue(str(ctx.exception).strip())

            self.assertEqual(state.list("factory"), [])
            for index in range(len(invalid_dags)):
                self.assertIsNone(state.get("factory", f"broken_{index}"))

            # An invalid dag leaves nothing on disk either.
            reloaded = HarnessState(state.file_path)
            self.assertEqual(reloaded.list("factory"), [])

    def test_create_factory_with_valid_dag_stores_arguments_dag(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            entry = state.create_factory(
                "PR review sweep",
                "Sweep review across changed files.",
                id="pr_sweep",
                path="review",
                dag=FACTORY_DAG,
                metadata={"owner": "kernel"},
            )

            self.assertEqual(entry.kind, "factory")
            self.assertEqual(entry.arguments["dag"], FACTORY_DAG)
            self.assertEqual(state.get("factory", "pr_sweep").arguments["dag"], FACTORY_DAG)
            self.assertIn(entry, state.list("factory"))

            reloaded = HarnessState(state.file_path)
            self.assertEqual(reloaded.get("factory", "pr_sweep").arguments["dag"], FACTORY_DAG)

    def test_create_factory_with_machine_stores_arguments_machine(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            machine = {
                "states": [
                    {"id": "collect", "entry": True, "subagent": "researcher", "outputs": [{"name": "findings", "type": "text"}]},
                    {
                        "id": "fix",
                        "subagent": {"prompt": "Fix the findings."},
                        "max_entries": 3,
                        "inputs": [{"name": "draft", "type": "text", "from": "collect.findings"}],
                    },
                ],
                "transitions": [
                    {"from": "collect", "to": "fix", "when": {"output": "findings", "op": "exists"}},
                ],
            }

            entry = state.create_factory("Loop", "Fix-and-review loop.", id="loop", machine=machine)

            self.assertEqual(entry.arguments, {"machine": machine})
            self.assertEqual(state.get("factory", "loop").arguments["machine"], machine)

            # An invalid machine is rejected exactly like an invalid dag.
            with self.assertRaises(ValueError):
                state.create_factory("Broken", "Never stores.", id="broken", machine={"states": [{}]})
            self.assertIsNone(state.get("factory", "broken"))

            # Supplying both forms at once is a usage error, not a validation one.
            with self.assertRaisesRegex(ValueError, "not both"):
                state.create_factory("Both", "Never stores.", id="both", dag=FACTORY_DAG, machine=machine)
            self.assertIsNone(state.get("factory", "both"))

    def test_update_factory_with_machine_replaces_the_spec(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_factory("PR review sweep", "Sweep.", id="pr_sweep", dag=FACTORY_DAG)

            machine = {"states": [{"id": "solo", "entry": True, "subagent": "worker"}], "transitions": []}
            updated = state.update_factory("pr_sweep", "PR review sweep", "Sweep v2.", machine=machine)
            self.assertEqual(updated.version, 2)
            self.assertEqual(state.get("factory", "pr_sweep").arguments, {"machine": machine})

            # Omitting both forms still preserves the stored spec.
            state.update_factory("pr_sweep", "PR review sweep", "Sweep v3.")
            self.assertEqual(state.get("factory", "pr_sweep").arguments, {"machine": machine})

            with self.assertRaises(ValueError):
                state.update_factory("pr_sweep", "PR review sweep", "Bad machine.", machine={"states": "nope"})
            self.assertEqual(state.get("factory", "pr_sweep").content, "Sweep v3.")

    def test_update_factory_without_dag_preserves_stored_dag(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            entry = state.create_factory("PR review sweep", "Sweep review.", id="pr_sweep", dag=FACTORY_DAG)
            stored_at_create = dict(entry.arguments["dag"])

            updated = state.update_factory("pr_sweep", "PR review sweep", "Sweep review across PRs.")

            self.assertEqual(updated.version, 2)
            self.assertEqual(state.get("factory", "pr_sweep").arguments["dag"], FACTORY_DAG)

            replacement = {
                "nodes": [
                    {"id": "solo", "subagent": "worker", "outputs": [{"name": "report", "type": "text"}]}
                ]
            }
            state.update_factory("pr_sweep", "PR review sweep", "Sweep review across PRs.", dag=replacement)
            self.assertEqual(state.get("factory", "pr_sweep").arguments["dag"], replacement)

            with self.assertRaises(ValueError):
                state.update_factory("pr_sweep", "PR review sweep", "Bad dag.", dag={"nodes": []})
            self.assertEqual(state.get("factory", "pr_sweep").arguments["dag"], replacement)
            # The pre-update snapshot was taken before any replacement landed.
            self.assertEqual(stored_at_create, FACTORY_DAG)

    def test_delete_factory_removes_the_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_factory("PR review sweep", "Sweep review.", id="pr_sweep", dag=FACTORY_DAG)

            self.assertTrue(state.delete_factory("pr_sweep"))
            self.assertIsNone(state.get("factory", "pr_sweep"))
            self.assertFalse(state.delete_factory("pr_sweep"))

    def test_generic_create_and_update_validate_factory_arguments(self) -> None:
        # The shared write path must enforce the write-time dry run for every
        # factory writer, not only create_factory: a generic create/update (a
        # refinement edit) carrying an invalid spec must raise and never
        # persist an unusable factory entry.
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "rejected"):
                state.create("factory", "Broken", "content", id="broken", arguments={"dag": {"nodes": []}})
            self.assertEqual(state.list("factory"), [])

            # Neither form at all is rejected with the structural wording.
            with self.assertRaisesRegex(ValueError, "require a dag or machine object in arguments"):
                state.create("factory", "Empty", "content", id="empty", arguments={})
            self.assertEqual(state.list("factory"), [])

            # Both forms at once is rejected with the usage wording.
            with self.assertRaisesRegex(ValueError, "not both"):
                state.create(
                    "factory",
                    "Both",
                    "content",
                    id="both",
                    arguments={"dag": FACTORY_DAG, "machine": {"states": [{"id": "s", "entry": True, "subagent": "w"}]}},
                )
            self.assertEqual(state.list("factory"), [])

            # A valid spec stores through the generic path, and a generic
            # update that writes arguments validates the replacement.
            state.create("factory", "Sweep", "content", id="sweep", arguments={"dag": FACTORY_DAG})
            with self.assertRaisesRegex(ValueError, "rejected"):
                state.update("factory", "sweep", "Sweep", "content", arguments={"dag": {"nodes": []}})
            self.assertEqual(state.get("factory", "sweep").arguments["dag"], FACTORY_DAG)

            # An update that omits arguments preserves the stored spec.
            state.update("factory", "sweep", "Sweep", "content updated")
            self.assertEqual(state.get("factory", "sweep").arguments["dag"], FACTORY_DAG)

            # An invalid spec leaves nothing on disk either.
            reloaded = HarnessState(state.file_path)
            self.assertEqual(len(reloaded.list("factory")), 1)
            self.assertIsNone(reloaded.get("factory", "broken"))

    def test_generic_create_factory_without_arguments_is_rejected(self) -> None:
        # Regression (bot review): a generic create with NO arguments stored
        # an unusable factory (no dag, no machine) that run() later rejects;
        # a NEW factory requires its spec (updates may omit it and preserve).
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "require a dag or machine object in arguments"):
                state.create("factory", "No spec", "content", id="no_spec")
            self.assertEqual(state.list("factory"), [])
            self.assertIsNone(state.get("factory", "no_spec"))
            reloaded = HarnessState(state.file_path)
            self.assertEqual(reloaded.list("factory"), [])

    def test_create_factory_deep_copies_the_caller_spec(self) -> None:
        # Regression (bot review): storing the caller-owned spec by reference
        # let a later mutation change the live entry without validation (and
        # an update that omits both forms would then preserve the mutated,
        # unvalidated spec).
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            dag = {"nodes": [{"id": "collect", "subagent": "worker"}]}
            state.create_factory("Sweep", "Sweep review.", id="sweep", dag=dag)

            dag["nodes"] = []  # corrupt the caller-owned dict after creation
            self.assertEqual(
                state.get("factory", "sweep").arguments["dag"],
                {"nodes": [{"id": "collect", "subagent": "worker"}]},
            )
            # An update that omits both forms preserves the VALID stored spec,
            # not the corrupted caller dict.
            state.update_factory("sweep", "Sweep", "Sweep review across PRs.")
            self.assertEqual(
                state.get("factory", "sweep").arguments["dag"],
                {"nodes": [{"id": "collect", "subagent": "worker"}]},
            )
            reloaded = HarnessState(state.file_path)
            self.assertEqual(
                reloaded.get("factory", "sweep").arguments["dag"],
                {"nodes": [{"id": "collect", "subagent": "worker"}]},
            )

    def test_save_failure_preserves_previous_state_on_disk(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_prompt_note("Durable fact", "Written before the crash.")

            crashing = HarnessState(state.file_path)
            original_dump = json.dump

            def torn_dump(data: object, fh: object, **kwargs: object) -> None:
                fh.write('{"schema": 1, "entr')  # type: ignore[attr-defined]
                raise OSError("disk full")

            json.dump = torn_dump  # type: ignore[assignment]
            try:
                with self.assertRaises(OSError):
                    crashing.create_prompt_note("Doomed fact", "Interrupted mid-write.")
            finally:
                json.dump = original_dump

            # The interrupted save must not have truncated the durable state.
            reloaded = HarnessState(state.file_path)
            titles = [entry.title for entry in reloaded.entries["prompt"].values()]
            self.assertEqual(titles, ["Durable fact"])

    @unittest.skipIf(os.name == "nt", "POSIX mode bits and umask")
    def test_save_preserves_existing_mode_despite_umask(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_prompt_note("First", "Creates the file.")
            os.chmod(state.file_path, 0o666)
            previous_umask = os.umask(0o022)
            try:
                state.create_prompt_note("Second", "Replaces the file.")
            finally:
                os.umask(previous_umask)

            self.assertEqual(os.stat(state.file_path).st_mode & 0o777, 0o666)

    @unittest.skipIf(os.name == "nt", "POSIX mode bits and umask")
    def test_save_new_file_keeps_restrictive_umask(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            previous_umask = os.umask(0o777)
            try:
                state.create_prompt_note("First", "Creates the file.")
            finally:
                os.umask(previous_umask)

            self.assertEqual(os.stat(state.file_path).st_mode & 0o777, 0)

    @unittest.skipIf(os.name == "nt", "POSIX mode bits and umask")
    def test_save_preserves_restrictive_file_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_prompt_note("First", "Creates the file.")
            os.chmod(state.file_path, 0o600)

            state.create_prompt_note("Second", "Replaces the file.")

            self.assertEqual(os.stat(state.file_path).st_mode & 0o777, 0o600)

    @unittest.skipIf(os.name == "nt", "POSIX mode bits and umask")
    def test_save_temp_file_is_never_looser_than_the_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_prompt_note("First", "Creates the file.")
            os.chmod(state.file_path, 0o600)

            observed_modes: list[int] = []
            original_open = os.open

            def observing_open(path: object, flags: int, mode: int = 0o777, **kwargs: object) -> int:
                if str(path).endswith(".tmp"):
                    observed_modes.append(mode)
                return original_open(path, flags, mode, **kwargs)

            os.open = observing_open  # type: ignore[assignment]
            try:
                state.create_prompt_note("Second", "Replaces the file.")
            finally:
                os.open = original_open

            self.assertEqual(observed_modes, [0o600])
            self.assertEqual(os.stat(state.file_path).st_mode & 0o777, 0o600)

    def test_save_writes_through_a_symlinked_state_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            real_path = Path(temp_dir) / "real_state.json"
            alias = Path(temp_dir) / "harness_state.json"
            HarnessState(real_path).create_prompt_note("Seed", "Creates the real file.")
            alias.symlink_to(real_path)

            state = HarnessState(alias)
            state.create_prompt_note("Through alias", "Must land in the real file.")

            self.assertTrue(alias.is_symlink())
            titles = [entry.title for entry in HarnessState(real_path).entries["prompt"].values()]
            self.assertEqual(titles, ["Seed", "Through alias"])

    def test_concurrent_subprocess_creates_all_land(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness" / "harness_state.json"
            start = Path(temp_dir) / "start"
            env = dict(os.environ)
            env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1] / "src")
            workers = []
            for worker in range(8):
                # Unbounded lock retries: on a loaded runner eight contending writers can outlast the
                # production ~1s acquire budget; this test proves no update is lost, not that budget.
                # The package rebinds `rlm.harness` to the facade, so patch the module itself.
                code = (
                    "import time\n"
                    "from importlib import import_module\n"
                    "from pathlib import Path\n"
                    "import_module('rlm.harness')._LOCK_ATTEMPTS = 10**9\n"
                    "from rlm.harness import HarnessState\n"
                    f"while not Path({str(start)!r}).exists():\n"
                    "    time.sleep(0.005)\n"
                    f"state = HarnessState({str(state_path)!r})\n"
                    "for i in range(10):\n"
                    f"    state.create('prompt', f'note {worker} {{i}}', f'note {worker} {{i}}', id=f'm-{worker}-{{i}}')\n"
                )
                workers.append(
                    subprocess.Popen([sys.executable, "-c", code], env=env)
                )
            start.write_text("", encoding="utf-8")
            for worker in workers:
                self.assertEqual(worker.wait(), 0)
            entries = HarnessState(state_path).list("prompt")
            self.assertEqual(len(entries), 80)

    def test_held_lock_dir_defers_writes_and_pins_the_shared_lock_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness" / "harness_state.json"
            state = HarnessState(state_path)
            state.create("prompt", "Seed", "Creates the file.", id="seed")
            lock_dir = Path(f"{os.path.realpath(state_path)}.lock")
            lock_dir.mkdir()
            try:
                with self.assertRaises(RuntimeError) as raised:
                    state.create("prompt", "Second", "Blocked by the lock.", id="second")
            finally:
                lock_dir.rmdir()
            self.assertIn(str(lock_dir), str(raised.exception))
            entries = HarnessState(state_path).list("prompt")
            self.assertEqual([entry.id for entry in entries], ["seed"])

    def test_save_syncs_parent_after_replace(self) -> None:
        if os.name != "posix":
            self.skipTest("directory fsync is POSIX-only")
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(path)
            events: list[str] = []
            replace, fsync = os.replace, os.fsync

            def observed_replace(source: Path, destination: Path) -> None:
                replace(source, destination)
                events.append("replace")

            def observed_fsync(fd: int) -> None:
                fsync(fd)
                events.append("directory sync" if stat.S_ISDIR(os.fstat(fd).st_mode) else "file sync")

            with patch.object(os, "replace", side_effect=observed_replace), patch.object(
                os, "fsync", side_effect=observed_fsync
            ):
                state.create_prompt_note("Durable", "Saved")
            self.assertEqual(events[-2:], ["replace", "directory sync"])

    def test_live_stale_lock_is_not_stolen(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)
            state.create_prompt_note("Seed", "Present")
            lock_dir = Path(f"{os.path.realpath(state_path)}.lock")
            owner = state._acquire_lock_dir(lock_dir)
            os.utime(lock_dir, (1, 1))
            try:
                with self.assertRaises(RuntimeError):
                    state.create_prompt_note("Second", "Blocked")
                self.assertTrue(state._owner_matches(lock_dir, owner))
            finally:
                (lock_dir / "owner").unlink()
                lock_dir.rmdir()
            self.assertEqual(len(HarnessState(state_path).list("prompt")), 1)

    @unittest.skipIf(os.name == "nt", "dead-owner reclaim relies on POSIX os.kill(pid, 0) semantics")
    def test_dead_lock_is_reclaimed_without_old_owner_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)
            state.create_prompt_note("Seed", "Present")
            lock_dir = Path(f"{os.path.realpath(state_path)}.lock")
            owner = state._acquire_lock_dir(lock_dir)
            worker = subprocess.Popen([sys.executable, "-c", "pass"])
            worker.wait()
            (lock_dir / "owner").write_text(f"{worker.pid} dead-token\n", encoding="utf-8")
            os.utime(lock_dir, (1, 1))
            state.create_prompt_note("Second", "Saved")
            self.assertFalse(state._owner_matches(lock_dir, owner))
            self.assertFalse(lock_dir.exists())
            self.assertEqual(len(HarnessState(state_path).list("prompt")), 2)

    def test_unparseable_stale_owner_is_reclaimed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)
            state.create_prompt_note("Seed", "Present")
            lock_dir = Path(f"{os.path.realpath(state_path)}.lock")
            lock_dir.mkdir()
            (lock_dir / "owner").write_bytes(b"\xff")
            os.utime(lock_dir, (1, 1))
            state.create_prompt_note("Second", "Saved")
            self.assertFalse(lock_dir.exists())
            self.assertEqual(len(HarnessState(state_path).list("prompt")), 2)

    def test_save_refuses_lost_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)
            state.create_prompt_note("Seed", "Present")
            with self.assertRaisesRegex(RuntimeError, "lock lost"):
                with state._locked():
                    lock_dir, _ = state._held_lock
                    (lock_dir / "owner").write_text("999999 other-token\n", encoding="utf-8")
                    state.save()
            self.assertTrue(lock_dir.exists())
            (lock_dir / "owner").unlink()
            lock_dir.rmdir()
            self.assertEqual(len(HarnessState(state_path).list("prompt")), 1)

    def test_load_ignores_unknown_json_keys(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state_path.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "entries": {
                            "skill": {
                                "known": {
                                    "id": "mismatched",
                                    "kind": "prompt",
                                    "title": "Known skill",
                                    "content": "Loaded despite extra keys.",
                                    "path": 123,
                                    "source": None,
                                    "version": "2",
                                    "metadata": "not a dict",
                                    "unexpected": True,
                                },
                                "missing_content": {
                                    "title": "Missing content",
                                }
                            }
                        },
                        "refinements": [
                            {
                                "id": "refine_extra",
                                "trigger": "extra keys",
                                "changes": [1, "loaded"],
                                "ignored": "value",
                            },
                            {
                                "id": "refine_missing_changes",
                                "trigger": "missing changes",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            state = HarnessState(state_path)

            self.assertEqual(state.get("skill", "known").content, "Loaded despite extra keys.")
            self.assertEqual(state.get("skill", "known").id, "known")
            self.assertEqual(state.get("skill", "known").kind, "skill")
            self.assertEqual(state.get("skill", "known").path, "general")
            self.assertEqual(state.get("skill", "known").source, "agent")
            self.assertIsNone(state.get("skill", "mismatched"))
            self.assertEqual(state.get("skill", "known").version, 2)
            self.assertEqual(state.get("skill", "known").metadata, {})
            self.assertIsNone(state.get("skill", "missing_content"))
            self.assertEqual(state.refinements[0].id, "refine_extra")
            self.assertEqual(state.refinements[0].changes, ["1", "loaded"])
            self.assertEqual(len(state.refinements), 1)
            self.assertIn("1, loaded", state.overview())

            updated = state.update("skill", "known", "Known skill", "Updated content.")
            self.assertEqual(updated.version, 3)

    def test_skill_arguments_are_first_class(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            created = state.create_skill(
                "Edit file",
                "Apply a targeted edit.",
                id="edit_file",
                reference={
                    "type": "python",
                    "import": "agent_skills.file_edit",
                    "callable": "file_edit",
                    "call_pattern": "await file_edit(path=..., find=..., replace=...)",
                },
                arguments={
                    "path": {"type": "string", "required": True},
                    "find": {"type": "string", "required": True},
                    "replace": {"type": "string", "required": True},
                },
            )
            updated = state.update_skill(
                "edit_file",
                "Edit file",
                "Apply a targeted edit after reading context.",
                reference={
                    "type": "python",
                    "import": "agent_skills.file_edit",
                    "callable": "file_edit",
                    "call_pattern": "await file_edit(path=..., find=..., replace=...)",
                },
                arguments={
                    "path": {"type": "string", "required": True},
                    "find": {"type": "string", "required": True},
                    "replace": {"type": "string", "required": True},
                    "validate": {"type": "boolean", "default": True},
                },
            )
            reloaded = HarnessState(state.file_path)

            self.assertEqual(created.arguments["path"]["required"], True)
            self.assertEqual(created.reference["type"], "python")
            self.assertEqual(updated.version, 2)
            self.assertEqual(reloaded.get("skill", "edit_file").arguments["validate"]["default"], True)
            self.assertEqual(reloaded.get("skill", "edit_file").reference["import"], "agent_skills.file_edit")
            self.assertIn('"path"', reloaded.overview())
            self.assertIn("agent_skills", reloaded.overview())

    def test_skill_references_must_be_python(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "Python reference"):
                state.create_skill("No reference", "missing", arguments={})
            with self.assertRaisesRegex(ValueError, "reference.type must be 'python'"):
                state.create_skill(
                    "Shell reference",
                    "bad",
                    reference={"type": "shell", "command": "edit"},
                    arguments={},
                )
            with self.assertRaisesRegex(ValueError, "Python import"):
                state.create_skill("No import", "bad", reference={"type": "python", "callable": "run"}, arguments={})
            with self.assertRaisesRegex(ValueError, "callable or call_pattern"):
                state.create_skill(
                    "No callable",
                    "bad",
                    reference={"type": "python", "import": "agent_skills.bad"},
                    arguments={},
                )

    def test_rejects_invalid_entry_fields_before_persisting(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)
            state.create_subagent("Valid", "seed content", id="valid_entry")
            seeded = state_path.read_text(encoding="utf-8")

            for label, kwargs, message in [
                ("content list", dict(title="T", content=["one string"]), "content must be a non-empty string, got a list"),
                ("title list", dict(title=["T"], content="c"), "title must be a non-empty string, got a list"),
                ("empty title", dict(title="", content="c"), "title must be a non-empty string, got an empty string"),
                ("numeric id", dict(title="T", content="c", id=7), "id must be a non-empty string, got int"),
                ("unhashable list id", dict(title="T", content="c", id=["x"]), "id must be a non-empty string, got a list"),
                ("falsy numeric id", dict(title="Zero", content="c", id=0), "id must be a non-empty string, got int"),
                ("numeric path", dict(title="T", content="c", path=7), "path must be a non-empty string, got int"),
                ("list metadata", dict(title="T", content="c", metadata=["m"]), "metadata must be a dict when provided, got a list"),
            ]:
                with self.subTest(case=f"create {label}"):
                    with self.assertRaisesRegex(ValueError, message):
                        state.create("subagent", **kwargs)
            with self.subTest(case="subagent with list content"):
                with self.assertRaisesRegex(ValueError, "content must be a non-empty string, got a list"):
                    state.create_subagent("T", ["one string"])
            with self.subTest(case="skill create without a reference"):
                with self.assertRaisesRegex(ValueError, "skill entries require a Python reference"):
                    state.create("skill", "Skill", "content", id="orphan_skill")
            with self.subTest(case="skill reference as a list"):
                with self.assertRaisesRegex(
                    ValueError, "skill entry 'Skill' rejected: skill entries require a Python reference"
                ):
                    state.create_skill("Skill", "content", reference=["bad"])
            with self.subTest(case="update with list content"):
                with self.assertRaisesRegex(ValueError, "content must be a non-empty string, got a list"):
                    state.update_subagent("valid_entry", "Valid", ["one string"])
                self.assertEqual(state_path.read_text(encoding="utf-8"), seeded)
            with self.subTest(case="invalid refinement events"):
                for label, kwargs, message in [
                    ("trigger list", dict(trigger=["t"], changes="ok"), "trigger must be a non-empty string, got a list"),
                    ("event id list", dict(trigger="t", changes="ok", id=["x"]), "id must be a non-empty string when provided, got a list"),
                    ("changes int", dict(trigger="t", changes=7), "changes must be a string or a list of strings, got int"),
                    ("mixed changes", dict(trigger="t", changes=["ok", 2]), "changes must be a list of non-empty strings"),
                    ("evidence list", dict(trigger="t", changes="ok", evidence=["e"]), "evidence must be a string"),
                    ("outcome int", dict(trigger="t", changes="ok", outcome=7), "outcome must be a string"),
                ]:
                    with self.subTest(case=label):
                        with self.assertRaisesRegex(ValueError, message):
                            state.record_refinement(**kwargs)

            reloaded = HarnessState(state_path)
            self.assertEqual([entry.id for entry in reloaded.list("subagent")], ["valid_entry"])
            self.assertEqual(reloaded.refinements, [])
            self.assertIsNone(reloaded.get("skill", "orphan_skill"))

    def test_load_tolerates_corrupt_or_non_object_state(self) -> None:
        for payload in ("not json at all", "null", "[]", '"a string"', "123"):
            with page_store(), tempfile.TemporaryDirectory() as temp_dir:
                state_path = Path(temp_dir) / "harness_state.json"
                state_path.write_text(payload, encoding="utf-8")

                state = HarnessState(state_path)

                self.assertEqual(state.list(), [])
                self.assertEqual(state.refinements, [])
                # The store must remain usable and self-heal on the next write.
                created = state.create_memory("Recovered", "Works after corruption.", id="recovered")
                self.assertEqual(HarnessState(state_path).get("memory", "recovered").content, created.content)

    def test_update_skill_preserves_omitted_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_skill(
                "Edit file",
                "Apply an edit.",
                id="edit_file",
                reference=PYTHON_REFERENCE,
                arguments={"path": {"type": "string", "required": True}},
            )

            # Updating only title/content (arguments omitted) must keep the contract.
            state.update_skill("edit_file", "Edit file", "Apply an edit carefully.", reference=PYTHON_REFERENCE)
            self.assertEqual(state.get("skill", "edit_file").arguments, {"path": {"type": "string", "required": True}})

            # An explicit empty dict still clears it.
            state.update_skill("edit_file", "Edit file", "Now argument-free.", reference=PYTHON_REFERENCE, arguments={})
            self.assertEqual(state.get("skill", "edit_file").arguments, {})

    def test_update_skill_without_reference_preserves_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_skill(
                "Edit file",
                "Apply an edit.",
                id="edit_file",
                reference=PYTHON_REFERENCE,
                arguments={"path": {"type": "string", "required": True}},
            )

            # A title/content-only update must not require re-sending the reference,
            # and must preserve the existing reference and arguments.
            updated = state.update_skill("edit_file", "Edit file", "Apply an edit carefully.")

            self.assertEqual(updated.version, 2)
            self.assertEqual(updated.reference, PYTHON_REFERENCE)
            self.assertEqual(updated.arguments, {"path": {"type": "string", "required": True}})
            self.assertEqual(updated.content, "Apply an edit carefully.")

    def test_update_preserves_omitted_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_prompt_note("Grouped", "content", id="grouped", path="repo/testing")

            # Updating without a path keeps the custom grouping path.
            state.update_prompt_note("grouped", "Grouped", "new content")
            self.assertEqual(state.get("prompt", "grouped").path, "repo/testing")

            # An explicit path still moves it.
            state.update_prompt_note("grouped", "Grouped", "newer", path="repo/other")
            self.assertEqual(state.get("prompt", "grouped").path, "repo/other")

    def test_in_memory_state_never_touches_disk(self) -> None:
        previous = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
            try:
                state = HarnessState(in_memory=True)
                created = state.create_prompt_note("Volatile", "in memory only", id="volatile")
                state.record_refinement("trigger", ["change"])

                self.assertIsNone(state.file_path)
                self.assertEqual(created.content, "in memory only")
                self.assertEqual(state.get("prompt", "volatile").content, "in memory only")
                # Local in-memory operations do not resolve or persist a path.
                self.assertEqual(list(Path(temp_dir).iterdir()), [])
            finally:
                if previous is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

    def test_in_memory_state_global_flag_uses_global_env_store(self) -> None:
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = HarnessState(in_memory=True)
                global_entry = state.create_skill(
                    "Global note",
                    "persisted",
                    id="global_note",
                    reference=PYTHON_REFERENCE,
                    global_=True,
                )
            finally:
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNone(state.file_path)
            self.assertEqual(global_entry.scope, "global")
            self.assertEqual(global_entry.content, "persisted")
            self.assertIsNone(state.get("skill", "global_note"))
            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("skill", "global_note").content,
                "persisted",
            )

    def test_in_memory_state_global_flag_uses_default_global_store(self) -> None:
        previous_agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            agent_dir = Path(temp_dir) / "agent"
            os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = str(agent_dir)
            os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
            try:
                state = HarnessState(in_memory=True)
                global_entry = state.create_skill(
                    "Default global",
                    "persisted",
                    id="default_global",
                    reference=PYTHON_REFERENCE,
                    global_=True,
                )
            finally:
                if previous_agent_dir is None:
                    os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
                else:
                    os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = previous_agent_dir
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNone(state.file_path)
            self.assertEqual(global_entry.scope, "global")
            self.assertIsNone(state.get("skill", "default_global"))
            self.assertEqual(
                HarnessState(agent_dir / "harness" / "harness_state.json", scope="global")
                .get("skill", "default_global")
                .content,
                "persisted",
            )

    def test_reloads_external_writes_before_mutating(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            kernel_state = HarnessState(state_path)
            kernel_state.create_prompt_note("Kernel note", "Written from the kernel.", id="kernel")

            # Simulate the host /refine command rewriting the same file from another
            # process. A second instance loads the current file, adds an entry, saves.
            host_state = HarnessState(state_path)
            host_state.create_prompt_note("Host note", "Written by /refine.", id="host")
            # Guarantee the mtime advances even on coarse-resolution filesystems.
            future = state_path.stat().st_mtime + 5
            os.utime(state_path, (future, future))

            # A read on the long-lived kernel state must observe the host write.
            self.assertEqual(kernel_state.get("prompt", "host").content, "Written by /refine.")

            # A mutation must merge onto the host write instead of clobbering it.
            kernel_state.create_prompt_note("Second kernel note", "Written later.", id="kernel_2")

            reloaded = HarnessState(state_path)
            self.assertIsNotNone(reloaded.get("prompt", "kernel"))
            self.assertIsNotNone(reloaded.get("prompt", "host"))
            self.assertIsNotNone(reloaded.get("prompt", "kernel_2"))

    def test_create_detects_externally_written_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)

            # Another process creates the same entry on disk after our last load.
            other = HarnessState(state_path)
            other.create_prompt_note("External", "Written elsewhere.", id="dup")
            future = state_path.stat().st_mtime + 5
            os.utime(state_path, (future, future))

            # create() must observe the external entry and honor create-or-fail.
            with self.assertRaisesRegex(ValueError, "already exists"):
                state.create_prompt_note("Local", "Should not overwrite.", id="dup")
            self.assertEqual(state.get("prompt", "dup").content, "Written elsewhere.")

    def test_explicit_create_and_update_enforce_entry_existence(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            first = state.create_skill("Triage", "old", id="triage", reference=PYTHON_REFERENCE, arguments={})
            with self.assertRaisesRegex(ValueError, "already exists"):
                state.create_skill("Triage", "duplicate", id="triage", reference=PYTHON_REFERENCE, arguments={})
            with self.assertRaisesRegex(ValueError, "does not exist"):
                state.update_skill("missing", "Missing", "missing", reference=PYTHON_REFERENCE, arguments={})

            second = state.update_skill("triage", "Triage", "new", reference=PYTHON_REFERENCE, arguments={})

            self.assertEqual(first.id, second.id)
            self.assertEqual(second.content, "new")
            self.assertEqual(second.version, 2)

    def test_explicit_state_dir_cache_uses_harness_state_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = get_harness_state(temp_dir)
            again = get_harness_state(temp_dir)

            self.assertIs(state, again)
            self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness_state.json")

    def test_explicit_state_dir_global_flag_uses_matching_state_file(self) -> None:
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            explicit_dir = Path(temp_dir) / "explicit"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                state = get_harness_state(explicit_dir)
                global_entry = state.create_skill(
                    "Scoped global",
                    "custom dir",
                    id="scoped_global",
                    reference=PYTHON_REFERENCE,
                    global_=True,
                )
            finally:
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(explicit_dir / "harness_state.json", scope="global").get("skill", "scoped_global")
            )
            self.assertFalse((env_global_dir / "harness_state.json").exists())

    def test_env_default_state_keeps_env_global_target_after_explicit_dir_cache_hit(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                cached_from_env = get_harness_state()
                # An explicit state_dir that aliases the env local dir must not
                # redirect the env-default singleton's global target.
                cached_from_explicit = get_harness_state(local_dir)
                global_entry = cached_from_env.create_skill(
                    "Env global",
                    "still targets the env global dir",
                    id="env_global_after_hit",
                    reference=PYTHON_REFERENCE,
                    global_=True,
                )
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIs(cached_from_env, cached_from_explicit)
            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(env_global_dir / "harness_state.json", scope="global").get(
                    "skill", "env_global_after_hit"
                )
            )
            self.assertIsNone(
                HarnessState(local_dir / "harness_state.json").get("skill", "env_global_after_hit")
            )

    def test_local_state_requires_local_path(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        try:
            os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            os.environ.pop("RLM_SESSION_DIR", None)
            with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                HarnessState()
        finally:
            if previous_local is None:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            else:
                os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
            if previous_session is None:
                os.environ.pop("RLM_SESSION_DIR", None)
            else:
                os.environ["RLM_SESSION_DIR"] = previous_session

    def test_default_state_uses_global_harness_env_dir(self) -> None:
        previous = os.environ.get("RLM_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            try:
                state = HarnessState()
            finally:
                if previous is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous

            self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness_state.json")

    def test_global_scope_default_state_uses_global_harness_env_dir(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = HarnessState(scope="global")
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(state.scope, "global")
            self.assertEqual(state.file_path, global_dir.resolve() / "harness_state.json")

    def test_default_state_is_local_and_global_flag_targets_global_store(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                global_state = get_harness_state(global_=True)
                local_entry = state.create_prompt_note("Local note", "Only this session.", id="local_note")
                global_entry = state.create_prompt_note("Global note", "All sessions.", id="global_note", global_=True)
                kwargs_entry = state.create_prompt_note(
                    "Kwargs global note",
                    "All sessions via kwargs.",
                    id="kwargs_global_note",
                    **{"global": True},
                )
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(state.file_path, local_dir.resolve() / "harness_state.json")
            self.assertEqual(global_state.file_path, global_dir.resolve() / "harness_state.json")
            self.assertEqual(local_entry.scope, "local")
            self.assertEqual(global_entry.scope, "global")
            self.assertEqual(kwargs_entry.scope, "global")
            self.assertIsNotNone(HarnessState(local_dir / "harness_state.json").get("prompt", "local_note"))
            self.assertIsNone(HarnessState(local_dir / "harness_state.json").get("prompt", "global_note"))
            self.assertIsNotNone(HarnessState(global_dir / "harness_state.json", scope="global").get("prompt", "global_note"))
            self.assertIsNotNone(
                HarnessState(global_dir / "harness_state.json", scope="global").get("prompt", "kwargs_global_note")
            )

    def test_global_kwarg_must_be_boolean(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(TypeError, "global must be a bool"):
                state.create_memory("Bad global flag", "bad", id="bad_global", **{"global": "false"})

    def test_state_cache_keeps_scope_distinct_when_local_and_global_share_a_file(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = temp_dir
            try:
                state = get_harness_state()
                global_state = get_harness_state(global_=True)
                local_entry = state.create_prompt_note("Local note", "Only this session.", id="local_note")
                global_entry = state.create_prompt_note("Global note", "All sessions.", id="global_note", global_=True)
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNot(state, global_state)
            self.assertEqual(state.file_path, global_state.file_path)
            self.assertEqual(state.scope, "local")
            self.assertEqual(global_state.scope, "global")
            self.assertEqual(local_entry.scope, "local")
            self.assertEqual(global_entry.scope, "global")
            reloaded = HarnessState(Path(temp_dir) / "harness_state.json")
            self.assertEqual(reloaded.get("prompt", "local_note").scope, "local")
            self.assertEqual(reloaded.get("prompt", "global_note").scope, "global")

    def test_scope_prefixed_ids_route_to_the_displayed_scope(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                state.create_prompt_note("Global note", "v1", id="routed", global_=True)

                # The overview displays [global:routed]; that id must be usable as-is
                # and imply the global scope without passing global_.
                updated = state.update_prompt_note("global:routed", "Global note", "v2")
                self.assertEqual(updated.scope, "global")
                self.assertEqual(state.get("prompt", "global:routed").content, "v2")
                self.assertIsNone(state.get("prompt", "routed"))

                state.create_prompt_note("Local note", "local", id="local_note")
                self.assertEqual(state.get("prompt", "local:local_note").content, "local")
                self.assertTrue(state.delete_prompt_note("local:local_note"))
                self.assertIsNone(state.get("prompt", "local_note"))
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("prompt", "routed").content,
                "v2",
            )

    def test_create_with_prefixed_id_does_not_mint_literal_id(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                entry = state.create_prompt_note("Validation", "content", id="global:validation")
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(entry.id, "validation")
            self.assertEqual(entry.scope, "global")
            global_store = HarnessState(global_dir / "harness_state.json", scope="global")
            self.assertIsNotNone(global_store.get("prompt", "validation"))
            self.assertIsNone(global_store.get("prompt", "global:validation"))
            self.assertFalse((local_dir / "harness_state.json").exists())

    def test_module_harness_binds_lazily_to_env_set_after_import(self) -> None:
        # Forkserver scenario: rlm is imported in the template process without the
        # per-session env; the child applies env after fork. rlm.harness must then
        # resolve against the new env instead of a store frozen at import time.
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            try:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                os.environ.pop("RLM_SESSION_DIR", None)
                # Without local env, local writes fail loudly instead of vanishing.
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires.*global_=True"):
                    package_harness.create_prompt_note("Volatile", "pre-env", id="pre_env")

                os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
                entry = package_harness.create_prompt_note("Session note", "persisted", id="session_note")
                self.assertIsNone(package_harness.get("prompt", "pre_env"))
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session

            self.assertEqual(entry.scope, "local")
            reloaded = HarnessState(Path(temp_dir) / "harness_state.json")
            self.assertEqual(reloaded.get("prompt", "session_note").content, "persisted")

    def test_dir_of_module_harness_lists_state_methods(self) -> None:
        self.assertIn("delete_factory", dir(package_harness))

    def test_module_harness_without_env_raises_on_local_writes_and_reads_work(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        with page_store():
            try:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                os.environ.pop("RLM_SESSION_DIR", None)

                for mutate in (
                    lambda: package_harness.create_prompt_note("Lost", "content", id="lost"),
                    lambda: package_harness.update_prompt_note("lost", "Lost", "content"),
                    lambda: package_harness.delete_prompt_note("lost"),
                    lambda: package_harness.upsert("skill", "Lost", "content", id="lost", reference=PYTHON_REFERENCE),
                    lambda: package_harness.record_refinement("trigger", ["change"]),
                ):
                    with self.assertRaisesRegex(RuntimeError, "Local harness state requires.*global_=True"):
                        mutate()

                # Reads keep working against an empty view.
                self.assertIsNone(package_harness.get("prompt", "lost"))
                self.assertEqual(package_harness.list(), [])
                self.assertIn("memory: 0", package_harness.overview())
                self.assertEqual(package_harness.snapshot()["refinements"], [])
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session

    def test_module_harness_without_env_still_routes_global_writes(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            global_dir = Path(temp_dir) / "global"
            try:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                os.environ.pop("RLM_SESSION_DIR", None)
                os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
                entry = package_harness.create_skill(
                    "Lesson",
                    "keep me",
                    id="no_session_lesson",
                    reference=PYTHON_REFERENCE,
                    global_=True,
                )
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(entry.scope, "global")
            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("skill", "no_session_lesson").content,
                "keep me",
            )

    def test_import_rlm_without_env_does_not_raise(self) -> None:
        env = dict(os.environ)
        env.pop("RLM_HARNESS_STATE_DIR", None)
        env.pop("RLM_SESSION_DIR", None)
        env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1] / "src")
        result = subprocess.run(
            [sys.executable, "-c", "import rlm; repr(rlm.harness); rlm.harness.overview(); rlm.harness.create_memory"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_local_state_dir_env_is_treated_as_unset(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            try:
                # Empty local dir must not fall through to the global agent-dir default.
                os.environ["RLM_HARNESS_STATE_DIR"] = ""
                os.environ.pop("RLM_SESSION_DIR", None)
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                    HarnessState()

                # With a session dir it takes the session fallback instead.
                os.environ["RLM_SESSION_DIR"] = temp_dir
                state = HarnessState()
                self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness" / "harness_state.json")

                # A whitespace-only session dir is also unset.
                os.environ["RLM_SESSION_DIR"] = "   "
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                    HarnessState()
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session

    def test_explicit_dir_aliasing_env_local_dir_keeps_env_global_target(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                # First construction happens via an explicit dir that merely aliases
                # the env local dir; global writes must still hit the env global dir.
                state = get_harness_state(local_dir)
                global_entry = state.create_prompt_note("Aliased", "still global", id="alias_global", global_=True)
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(env_global_dir / "harness_state.json", scope="global").get("prompt", "alias_global")
            )
            self.assertIsNone(
                HarnessState(local_dir / "harness_state.json").get("prompt", "alias_global")
            )

    def test_callable_rlm_exposes_harness_state_helpers(self) -> None:
        self.assertIs(callable_rlm.harness, package_harness)
        self.assertIs(callable_rlm.get_harness_state, get_harness_state)

    def test_record_refinement_accepts_single_change_string(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            event = state.record_refinement("manual cli test", "single change")

            self.assertEqual(event.changes, ["single change"])
            self.assertEqual(state.refinements[0].changes, ["single change"])

    def test_unknown_kind_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.upsert("tool", "Tool", "Tool content")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.get("tool", "tool")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.delete("tool", "tool")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.list("tool")




class HarnessMemoryPagesTest(unittest.TestCase):
    """Memory is a page store, not an entry kind: these cover the page contract."""

    def test_create_memory_writes_a_page_and_no_state_entry(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)

            entry = state.create_memory(
                "Tea notes",
                "All about oolong brewing.",
                id="tea",
                type="world",
                hook="read when brewing tea",
            )

            page = Path(entry.metadata["page"])
            self.assertTrue(page.exists())
            fields, body = harness_module._split_page(page.read_text(encoding="utf-8"))
            self.assertEqual(
                fields,
                {
                    "slug": "tea",
                    "type": "world",
                    "title": "Tea notes",
                    "status": "draft",
                    "updated": datetime.now(timezone.utc).date().isoformat(),
                    "hook": "read when brewing tea",
                },
            )
            self.assertEqual(body.strip(), "All about oolong brewing.")
            # The page is the store: nothing about it reaches the harness state file.
            state.create_prompt_note("Unrelated", "Writes the state file.")
            self.assertEqual(
                json.loads(state_path.read_text(encoding="utf-8"))["entries"]["memory"], {}
            )

    def test_update_memory_rewrites_the_page_it_found(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            created = state.create_memory("Tea notes", "First body.", id="tea")

            updated = state.update_memory("tea", "Tea notes", "Second body.", hook="read again")

            page = Path(updated.metadata["page"])
            self.assertEqual(page, Path(created.metadata["page"]))
            fields, body = harness_module._split_page(page.read_text(encoding="utf-8"))
            self.assertEqual(body.strip(), "Second body.")
            self.assertEqual(fields["status"], "active")
            self.assertEqual(fields["hook"], "read again")
            self.assertEqual(len(list(page.parent.glob("*.md"))), 1)

    def test_load_migrates_legacy_memory_entries_into_pages(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state_path.write_text(json.dumps(LEGACY_MEMORY_STATE), encoding="utf-8")

            state = HarnessState(state_path)

            page = Path(state.get("memory", "legacy_note").metadata["page"])
            fields, body = harness_module._split_page(page.read_text(encoding="utf-8"))
            self.assertEqual(fields["title"], "Legacy note")
            self.assertEqual(fields["type"], "world")
            self.assertEqual(fields["status"], "active")
            self.assertEqual(fields["hook"], "read when routed")
            self.assertEqual(body.strip(), "Carried over.")
            # The store the entries came from is kept beside the state file.
            backup = state_path.with_name("harness_state.pre-pages.json")
            self.assertEqual(json.loads(backup.read_text(encoding="utf-8")), LEGACY_MEMORY_STATE)
            # Only the save that follows the import stops writing the kind.
            state.create_prompt_note("After import", "Writes the state file.")
            self.assertEqual(
                json.loads(state_path.read_text(encoding="utf-8"))["entries"]["memory"], {}
            )

    def test_load_keeps_legacy_memory_entries_when_pages_already_exist(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            HarnessState(state_path).create_memory("Existing page", "Already here.", id="existing")
            state_path.write_text(json.dumps(LEGACY_MEMORY_STATE), encoding="utf-8")

            state = HarnessState(state_path)
            state.create_prompt_note("After load", "Writes the state file.")

            # A store that already has pages is never imported into, and an entry
            # the import skipped stays in the file it was read from.
            entries = json.loads(state_path.read_text(encoding="utf-8"))["entries"]
            self.assertEqual(list(entries["memory"]), ["legacy_note"])
            self.assertFalse(state_path.with_name("harness_state.pre-pages.json").exists())
            self.assertIsNone(state.get("memory", "legacy_note"))

    def test_create_memory_refuses_a_session_local_page(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "memory pages are global"):
                state.create_memory("Local note", "Session only.", id="local_note", global_=False)
            with self.assertRaisesRegex(ValueError, "memory pages are global"):
                state.create("memory", "Local note", "Session only.", id="local_note", global_=False)

            self.assertIsNone(state.get("memory", "local_note"))

    def test_session_local_state_still_writes_to_the_agent_dir_pages(self) -> None:
        with page_store() as agent_dir, tempfile.TemporaryDirectory() as temp_dir:
            state_dir = Path(temp_dir) / "session-artifacts" / "sid"
            state = HarnessState(state_dir / "harness" / "harness_state.json")

            entry = state.create_memory("Tea notes", "One store.", id="tea")

            page = Path(entry.metadata["page"])
            self.assertEqual(
                page, (agent_dir / "memory" / "pages" / "experience" / "tea.md").resolve()
            )
            self.assertTrue(page.exists())
            self.assertFalse((state_dir / "memory").exists())
            self.assertIn("tea", harness_module.memory_pages(agent_dir / "memory"))

    def test_delete_memory_refuses_a_page_with_evidence(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            page = Path(state.create_memory("Proven fact", "Short body.", id="proven").metadata["page"])
            text = page.read_text(encoding="utf-8")
            page.write_text(text.replace("---\n\n", "evidence:\n  - kind: run\n---\n\n", 1), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "carries evidence"):
                state.delete_memory("proven")

            self.assertTrue(page.exists())

    def test_delete_memory_refuses_a_recent_write(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            page = Path(state.create_memory("Fresh note", "Short body.", id="fresh").metadata["page"])

            with self.assertRaisesRegex(ValueError, "less than two weeks ago"):
                state.delete_memory("fresh")

            self.assertTrue(page.exists())

    def test_dream_due_waits_for_the_next_pass_the_journal_names(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Tea notes", "All about oolong brewing.", id="tea")
            journal = harness_module._dream_journal_path()
            journal.parent.mkdir(parents=True, exist_ok=True)

            now = int(datetime.now(timezone.utc).timestamp())
            journal.write_text(json.dumps({"next_at": now + 3600}) + "\n", encoding="utf-8")
            self.assertFalse(state.dream_due()[0])

            journal.write_text(json.dumps({"next_at": now - 3600}) + "\n", encoding="utf-8")
            self.assertEqual(state.dream_due(), (True, "the last pass is ready for another"))

    def test_record_dream_pass_schedules_the_next_pass_inside_the_allowed_window(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Tea notes", "All about oolong brewing.", id="tea")

            for next_hours, floor_or_ceiling in ((1, 6), (10**6, 14 * 24)):
                record = state.record_dream_pass(visited=3, next_hours=next_hours)

                self.assertEqual(record["visited"], 3)
                self.assertEqual(record["next_at"] - record["ts"], floor_or_ceiling * 3600)
                self.assertFalse(state.dream_due()[0])

    def test_update_memory_moving_the_type_leaves_one_page(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            created = Path(state.create_memory("Tea notes", "First body.", id="tea").metadata["page"])
            self.assertEqual(created.parent.name, "experience")

            updated = state.update_memory("tea", "Tea notes", "First body.", metadata={"type": "model"})

            page = Path(updated.metadata["page"])
            self.assertEqual(page.parent.name, "model")
            self.assertFalse(created.exists(), "the old type's copy would win by directory order")
            self.assertEqual(
                [p.relative_to(page.parents[1]).as_posix() for p in page.parents[1].rglob("tea.md")],
                ["model/tea.md"],
            )
            self.assertEqual(harness_module._page_files(page.parents[2])["tea"], page)

    def test_promotion_needs_the_evidence_the_next_type_demands(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            once = Path(state.create_memory("Once", "Body.", id="once").metadata["page"])
            self.assertIsNone(harness_module._promotion_verdict("once", once.parents[2])[0])

            once.write_text(
                once.read_text(encoding="utf-8").replace(
                    "---\n\n",
                    'evidence:\n  - {kind: session, path: "a"}\n  - {kind: file, path: "b"}\n---\n\n',
                    1,
                ),
                encoding="utf-8",
            )
            self.assertEqual(harness_module._promotion_verdict("once", once.parents[2])[0], "observation")

            self.assertTrue(harness_module._promote_memory_page("once", "observation", once.parents[2]))
            promoted = harness_module._page_files(once.parents[2])["once"]
            self.assertEqual(promoted.parent.name, "observation")
            self.assertFalse(once.exists())
            fields, _body = harness_module._split_page(promoted.read_text(encoding="utf-8"))
            self.assertEqual(fields["type"], "observation")
            self.assertIn("from: experience, to: observation", promoted.read_text(encoding="utf-8"))

            # One step at a time: the target must be the step the evidence earned.
            self.assertFalse(harness_module._promote_memory_page("once", "world", once.parents[2]))
            # observation -> model needs two kinds of evidence, which two entries share here.
            self.assertEqual(
                harness_module._promotion_verdict("once", once.parents[2])[0], "model"
            )

    def test_promotion_refuses_a_disputed_page_and_stops_at_world(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            page = Path(state.create_memory("Claim", "Body.", id="claim").metadata["page"])
            page.write_text(
                page.read_text(encoding="utf-8")
                .replace("status: draft", "status: disputed")
                .replace(
                    "---\n\n",
                    'evidence:\n  - {kind: cmd, path: "x"}\n  - {kind: web, path: "y"}\n---\n\n',
                    1,
                ),
                encoding="utf-8",
            )
            verdict, why = harness_module._promotion_verdict("claim", page.parents[2])
            self.assertIsNone(verdict)
            self.assertIn("disputed", why)

    def test_merge_pages_carries_evidence_entries_across(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            kept_page = Path(state.create_memory("Alpha", "Alpha body.", id="alpha").metadata["page"])
            absorbed_page = Path(state.create_memory("Beta", "Beta body.", id="beta").metadata["page"])
            kept_page.write_text(
                kept_page.read_text(encoding="utf-8").replace(
                    "---\n\n", 'evidence:\n  - kind: run\n    source: "alpha-run"\n---\n\n', 1
                ),
                encoding="utf-8",
            )
            # Column-zero list items are the other style a page may use.
            absorbed_page.write_text(
                absorbed_page.read_text(encoding="utf-8").replace(
                    "---\n\n", 'evidence:\n- kind: rerun\n  source: "beta-run"\n---\n\n', 1
                ),
                encoding="utf-8",
            )

            self.assertTrue(state.merge_pages("alpha", "beta"))

            text = kept_page.read_text(encoding="utf-8")
            self.assertIn('  - kind: run\n    source: "alpha-run"', text)
            self.assertIn('  - kind: rerun\n    source: "beta-run"', text)
            self.assertIn("Beta body.", text)
            self.assertFalse(absorbed_page.exists())
            self.assertTrue((kept_page.parents[2] / "archive" / "beta.md").exists())


class HarnessReadPageTest(unittest.TestCase):
    """read_page is the walk entry point: body, edges out, backlinks in, read recorded."""

    def test_read_page_returns_body_edges_and_backlinks(self) -> None:
        with page_store():
            state = HarnessState(Path("unused_state.json"))
            state.create_memory(
                "Hub", "The hub body.", id="hub", type="model", hook="read when hub",
            )
            state.create_memory(
                "Spoke", "The spoke body.", id="spoke", type="observation", hook="read when spoke",
            )
            spoke_page = Path(harness_module.memory_pages()["spoke"].metadata["page"])
            text = spoke_page.read_text(encoding="utf-8")
            fields, body = harness_module._split_page(text)
            frontmatter = harness_module._frontmatter_lines(text)
            merged = harness_module._merge_frontmatter(frontmatter, {"derived_from": "hub"})
            spoke_page.write_text(
                "---\n" + "\n".join(merged) + "\n---\n\n" + body.strip() + "\n", encoding="utf-8"
            )

            page = harness_module.read_page("hub")
            self.assertEqual(page["body"], "The hub body.")
            self.assertEqual(page["out_edges"], {})
            self.assertEqual(page["backlinks"], {"derived_from": ["spoke"]})

            spoke = harness_module.read_page("spoke")
            self.assertEqual(spoke["out_edges"], {"derived_from": ["hub"]})
            self.assertEqual(spoke["backlinks"], {})

    def test_read_page_records_the_read_for_the_decay_ladder(self) -> None:
        with page_store():
            HarnessState(Path("unused_state.json")).create_memory(
                "Tea", "Oolong notes.", id="tea", type="model", hook="read when brewing",
            )
            today = datetime.now(timezone.utc).date().isoformat()
            harness_module.read_page("tea")
            text = Path(harness_module.memory_pages()["tea"].metadata["page"]).read_text(
                encoding="utf-8"
            )
            fields, _ = harness_module._split_page(text)
            self.assertEqual(fields.get("last_read"), today)
            self.assertEqual(fields.get("weight"), "1")

    def test_read_page_unknown_slug_is_none(self) -> None:
        with page_store():
            self.assertIsNone(harness_module.read_page("no-such-page"))


class HarnessSearchTest(unittest.TestCase):
    def test_search_ranks_relevant_entries_first(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Tea notes", "All about oolong brewing.", id="tea")
            state.create_memory("Worktree policy", "Use git worktrees for parallel branches.", id="worktree")
            state.create_prompt_note("RSI program", "Ship [RSI] PRs from worktrees.", id="rsi")

            results = state.search("worktree branches")

            self.assertTrue(results)
            self.assertEqual(results[0].id, "worktree")
            self.assertTrue(all(entry.id != "tea" for entry in results))

    def test_search_filters_by_kind_and_limit(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Worktree memory", "worktree workflow", id="m1")
            state.create_prompt_note("Worktree prompt", "worktree workflow", id="p1")
            state.create_prompt_note("Worktree prompt 2", "worktree workflow", id="p2")

            prompts = state.search("worktree", kind="prompt")
            self.assertTrue(prompts)
            self.assertEqual({entry.kind for entry in prompts}, {"prompt"})

            limited = state.search("worktree", kind="prompt", limit=1)
            self.assertEqual(len(limited), 1)

    def test_search_matches_non_ascii_queries(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Tokyo note", "東京ミーティングの議事録。", id="tokyo")

            results = state.search("東京")
            self.assertEqual([entry.id for entry in results], ["tokyo"])

    def test_search_segments_whitespace_free_cjk(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Login fix", "登录故障排查记录。", id="login")
            state.create_memory("Tea notes", "All about oolong brewing.", id="tea")

            results = state.search("修复登录")

            self.assertEqual([entry.id for entry in results], ["login"])

    def test_search_matches_supplementary_cjk(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Ext B note", "𠀀𠀁 ideographs recorded.", id="extb")

            self.assertEqual([entry.id for entry in state.search("𠀀")], ["extb"])
            self.assertEqual([entry.id for entry in state.search("𠀀𠀁")], ["extb"])

    def test_search_keeps_accented_latin_words_whole(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Review notes", "The naïve approach failed.", id="naive")

            results = state.search("naïve")

            self.assertEqual([entry.id for entry in results], ["naive"])

    def test_search_matches_combining_mark_scripts(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Book note", "किताब पढ़ रहा हूँ।", id="book")

            results = state.search("किताब")

            self.assertEqual([entry.id for entry in results], ["book"])

    def test_search_drops_single_character_non_cjk_terms(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Russian note", "мир и согласие в команде.", id="mir")

            with self.assertRaisesRegex(ValueError, "no searchable term"):
                state.search("и")
            self.assertEqual([entry.id for entry in state.search("мир")], ["mir"])

    def test_search_treats_punctuation_as_separators(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Branch hygiene", "Use git worktrees for parallel branches.", id="worktree")
            state.create_memory("Question", "Anything else left open?", id="question")

            results = state.search("worktree?")
            self.assertEqual([entry.id for entry in results], ["worktree"])

            with self.assertRaisesRegex(ValueError, "no searchable term"):
                state.search("??? / . ,")

    def test_search_drops_zero_score_entries_and_validates_args(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Tea notes", "All about oolong brewing.", id="tea")

            self.assertEqual(state.search("quantum"), [])
            with self.assertRaisesRegex(ValueError, "no searchable term"):
                state.search("   ")
            with self.assertRaises(TypeError):
                state.search(42)
            with self.assertRaises(TypeError):
                state.search("worktree", limit=0)

    def test_search_discounts_common_terms_and_keeps_frequency_ties(self) -> None:
        with page_store(), tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            # An empty corpus scores nothing, and a lone entry (N=1, df=1 -> log(2)) still scores.
            self.assertEqual(state.search("session"), [])
            state.create_prompt_note("Session notes", "Session signal.", id="solo")
            self.assertEqual([entry.id for entry in state.search("session")], ["solo"])
            state.entries["prompt"]["solo"].updated_at = "2026-07-01T00:00:00+00:00"

            # Equal frequency discounts every match alike, so recency still orders them: id order
            # alone would put aa_older first.
            for entry_id, day in (("aa_older", "08-01"), ("zz_newer", "09-01")):
                state.create_prompt_note("Session notes", "Same session signal.", id=entry_id)
                state.entries["prompt"][entry_id].updated_at = f"2026-{day}T00:00:00+00:00"
            self.assertEqual([entry.id for entry in state.search("session")], ["zz_newer", "aa_older", "solo"])

            # "session" matches 3 of 4 (log(1 + 4/3)), "quantum" 1 of 4 (log(1 + 4)): rare ranks first.
            state.create_prompt_note("Quantum note", "Only quantum annealing matters once.", id="rare")
            state.entries["prompt"]["rare"].updated_at = "2026-07-01T00:00:00+00:00"
            self.assertEqual(
                [entry.id for entry in state.search("session quantum")], ["rare", "zz_newer", "aa_older", "solo"]
            )
    def test_disabled_harness_keeps_kernel_api_introspection_available(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": tmp}):
            (Path(tmp) / "settings.json").write_text('{"harnessRefinements":false}')
            # The real kernel bootstrap probes these methods before any cell runs.
            for method in ("create_memory", "update_memory", "delete_memory", "create_skill", "record_refinement"):
                self.assertTrue(callable(getattr(package_harness, method)))
                self.assertTrue(callable(getattr(callable_rlm.harness, method)))
            with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                package_harness.create_prompt_note("No", "Do not create")
            with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                package_harness.snapshot()

    def test_project_settings_cannot_override_the_global_master_switch(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent_dir = Path(tmp) / "agent"
            project_dir = Path(tmp) / "project" / ".prime" / "agent"
            agent_dir.mkdir()
            project_dir.mkdir(parents=True)
            (agent_dir / "settings.json").write_text('{"harnessRefinements":false}')
            project_settings = project_dir / "settings.json"
            project_settings.write_text('{"harnessRefinements":true}')
            with patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": str(agent_dir), "PRIME_AGENT_SETTINGS_CWD": str(Path(tmp) / "project")}):
                with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                    HarnessState(Path(tmp) / "harness_state.json", scope="global")
                (agent_dir / "settings.json").write_text('{"harnessRefinements":true}')
                project_settings.write_text('{"harnessRefinements":false}')
                state = HarnessState(Path(tmp) / "harness_state.json", scope="global")
                state.create_prompt_note("Note", "Global preference wins", id="note")
                self.assertEqual(state.get("prompt", "note").content, "Global preference wins")
                (agent_dir / "settings.json").write_text('{"harnessRefinements":false}')
                project_settings.write_text('{"harnessRefinements":true}')
                with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                    state.get("prompt", "note")
                (agent_dir / "settings.json").write_text('{"harnessRefinements":"false"}')
                self.assertEqual(state.get("prompt", "note").content, "Global preference wins")

    def test_disable_blocks_cached_reads_and_writes_without_deleting_state(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": tmp}):
            settings = Path(tmp) / "settings.json"
            settings.write_text('{"harnessRefinements":true}')
            state = get_harness_state(tmp, global_=True)
            state.create_skill("Note", "Saved skill", id="note", reference=PYTHON_REFERENCE)
            before = state.file_path.read_bytes()
            settings.write_text('{"harnessRefinements":false}')
            for action in (
                lambda: get_harness_state(tmp, global_=True),
                lambda: state.get("skill", "note"),
                lambda: state.create_skill("New", "New skill", id="new", reference=PYTHON_REFERENCE),
                state.load,
                state.save,
            ):
                with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                    action()
            self.assertEqual(state.file_path.read_bytes(), before)
            settings.write_text('{"harnessRefinements":true}')
            self.assertEqual(state.get("skill", "note").content, "Saved skill")

    def test_disabled_refinements_refuse_factory_runs_before_launch(self):
        import asyncio
        import importlib

        factory = importlib.import_module("rlm.factory")

        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": tmp}):
            settings = Path(tmp) / "settings.json"
            settings.write_text('{"factory":{"enabled":true},"harnessRefinements":true}')
            state = HarnessState(Path(tmp) / "harness_state.json")
            state.create_factory("Saved factory", "Saved spec", id="saved", dag=FACTORY_DAG)
            before = state.file_path.read_bytes()
            executor = factory.FactoryExecutor(harness=state)
            settings.write_text('{"factory":{"enabled":true},"harnessRefinements":false}')
            with patch.object(factory, "_DEFAULT_EXECUTOR", executor):
                for name in ("saved", "review-sweep"):
                    with self.assertRaisesRegex(RuntimeError, "disabled in /settings"):
                        asyncio.run(factory.run_factory(name))
            self.assertEqual(executor._runs, {})
            self.assertEqual(state.file_path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
