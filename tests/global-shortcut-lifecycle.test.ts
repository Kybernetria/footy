import { expect, test } from "bun:test";
import { runGlobalShortcutBindingOperation } from "../src/components/settings/globalShortcutLifecycle";

test("resumes only bindings that were suspended successfully", async () => {
  const suspended = await runGlobalShortcutBindingOperation(
    ["working", "rejected"],
    async (id) =>
      id === "working"
        ? { status: "ok", data: null }
        : { status: "error", error: "already unavailable" },
  );
  const resumed: string[] = [];

  await runGlobalShortcutBindingOperation(
    suspended.successfulIds,
    async (id) => {
      resumed.push(id);
      return { status: "ok", data: null };
    },
  );

  expect(suspended.successfulIds).toEqual(["working"]);
  expect(suspended.errors).toEqual([
    { id: "rejected", error: "already unavailable" },
  ]);
  expect(resumed).toEqual(["working"]);
});

test("treats thrown command failures as operation errors", async () => {
  const result = await runGlobalShortcutBindingOperation(
    ["binding"],
    async () => {
      throw new Error("backend unavailable");
    },
  );

  expect(result.successfulIds).toEqual([]);
  expect(result.errors[0]?.id).toBe("binding");
  expect(result.errors[0]?.error).toEqual(new Error("backend unavailable"));
});
