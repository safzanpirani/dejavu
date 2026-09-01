export const DEFAULT_MAX_PARALLEL = 4;

export async function mapPool<T, U>(
  items: readonly T[],
  maxParallel: number,
  mapper: (item: T, index: number) => Promise<U>,
): Promise<U[]> {
  if (!Number.isInteger(maxParallel) || maxParallel < 1) {
    throw new Error(`maxParallel must be an integer >= 1 (got '${maxParallel}')`);
  }
  const results = new Array<U>(items.length);
  let nextIndex = 0;
  const workers = Array.from({ length: Math.min(maxParallel, items.length) }, async () => {
    for (;;) {
      const index = nextIndex++;
      if (index >= items.length) return;
      results[index] = await mapper(items[index]!, index);
    }
  });
  await Promise.all(workers);
  return results;
}
