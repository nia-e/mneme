"""Pure policy fixtures; no files, native owner or provider required."""
from dataclasses import FrozenInstanceError
from types import SimpleNamespace
import unittest
from librarian_policy import EFFORTS, MODEL, LibrarianBudget, resolve

class PolicyTests(unittest.TestCase):
    def test_exact_explicit_presets_and_medium_continuity(self):
        expected=[(8192,2048,10,32768,24,125000,10000),(12288,4096,15,65536,48,250000,20000),(24576,8192,20,131072,96,500000,40000)]
        for effort,values in zip(EFFORTS,expected):
            budget=resolve({"reader_model":MODEL,"librarian_effort":effort})
            self.assertEqual(tuple(getattr(budget,key) for key in ("selector_prompt_bytes","selector_answer_bytes","native_seconds","native_read_bytes","attempts","input_tokens","output_tokens")),values)
            self.assertEqual(budget.routing_prompt_bytes,min(values[0],12288))
            self.assertEqual(budget.routing_answer_bytes,min(values[1],4096))
            self.assertEqual(budget.model,MODEL);self.assertEqual(budget.effort,effort)
            self.assertEqual(resolve(SimpleNamespace(reader_model=MODEL,librarian_effort=effort)),budget)
        self.assertEqual(LibrarianBudget(),resolve({"reader_model":MODEL,"librarian_effort":"medium"}))
    def test_no_config_defaults_or_legacy_model_fallback(self):
        for value in (None,False,True,1,[],{},"", "gpt-5.6-sol", "alternate"):
            with self.subTest(model=value),self.assertRaises(ValueError):resolve({"reader_model":value,"librarian_effort":"medium"})
        for value in (None,False,True,1,[],{},"", "MEDIUM", "xhigh"):
            with self.subTest(effort=value),self.assertRaises(ValueError):resolve({"reader_model":MODEL,"librarian_effort":value})
        for config in ({},{"reader_model":MODEL},{"librarian_effort":"medium"},None):
            with self.assertRaises(ValueError):resolve(config)
    def test_frozen_policy_has_no_mutable_resource_overrides(self):
        budget=LibrarianBudget()
        with self.assertRaises(FrozenInstanceError):budget.effort="high"
        with self.assertRaises((AttributeError,TypeError)):budget.attempts=999
        with self.assertRaises(TypeError):LibrarianBudget(attempts=999)

if __name__=="__main__":unittest.main()
