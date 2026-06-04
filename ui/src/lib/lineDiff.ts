export type DiffOp = { type: "add" | "del" | "context"; value: string };

/**
 * Minimal LCS-based line diff (no external dep). Produces a unified op list
 * suitable for a git-style red/green render. Fine for proposal-spec sizes.
 */
export function lineDiff(a: string, b: string): DiffOp[] {
  const aLines = a.split("\n");
  const bLines = b.split("\n");
  const n = aLines.length;
  const m = bLines.length;

  // dp[i][j] = LCS length of aLines[i..] and bLines[j..]
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] =
        aLines[i] === bLines[j]
          ? dp[i + 1][j + 1] + 1
          : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }

  const ops: DiffOp[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (aLines[i] === bLines[j]) {
      ops.push({ type: "context", value: aLines[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      ops.push({ type: "del", value: aLines[i] });
      i++;
    } else {
      ops.push({ type: "add", value: bLines[j] });
      j++;
    }
  }
  while (i < n) ops.push({ type: "del", value: aLines[i++] });
  while (j < m) ops.push({ type: "add", value: bLines[j++] });
  return ops;
}

export function diffStats(ops: DiffOp[]): { added: number; removed: number } {
  return {
    added: ops.filter((o) => o.type === "add").length,
    removed: ops.filter((o) => o.type === "del").length,
  };
}
