export type BindingCommandResult =
  | { status: "ok"; data?: unknown }
  | { status: "error"; error: unknown };

export interface BindingOperationError {
  id: string;
  error: unknown;
}

export interface BindingOperationSummary {
  successfulIds: string[];
  errors: BindingOperationError[];
}

export const runGlobalShortcutBindingOperation = async (
  ids: string[],
  operation: (id: string) => Promise<BindingCommandResult>,
): Promise<BindingOperationSummary> => {
  const outcomes = await Promise.all(
    ids.map(async (id) => {
      try {
        const result = await operation(id);
        return result.status === "ok"
          ? { id, success: true as const }
          : { id, success: false as const, error: result.error };
      } catch (error) {
        return { id, success: false as const, error };
      }
    }),
  );

  return {
    successfulIds: outcomes
      .filter((outcome) => outcome.success)
      .map((outcome) => outcome.id),
    errors: outcomes
      .filter(
        (outcome): outcome is { id: string; success: false; error: unknown } =>
          !outcome.success,
      )
      .map(({ id, error }) => ({ id, error })),
  };
};
