// What a start URL names, read the way Henk reads it (crates/henk/src/urls.rs),
// so the start dialog can say so while you type (#230). Only a help: the
// server checks again and its answer counts.

/** A pull or merge request, or an issue. */
export interface Target {
  kind: 'pull' | 'issue';
  platform: 'github' | 'gitlab';
  /** `owner/name`, or a GitLab project path. */
  repo: string;
  number: number;
}

/** What can be started, and what each needs. */
export type Start = 'review' | 'plan' | 'address';

function split(url: string): { host: string; path: string } | null {
  const text = url.trim();
  const rest = text.startsWith('https://')
    ? text.slice('https://'.length)
    : text.startsWith('http://')
      ? text.slice('http://'.length)
      : null;
  if (rest === null) return null;
  const slash = rest.indexOf('/');
  if (slash < 0) return null;
  return { host: rest.slice(0, slash), path: rest.slice(slash + 1).replace(/\/+$/, '') };
}

/** A whole number as Rust's `u64::parse` takes it: digits only. */
function number(text: string | undefined): number | null {
  return text !== undefined && /^\d+$/.test(text) ? Number(text) : null;
}

/** The target `url` names, or null when Henk would not read it as one. */
export function parseTarget(url: string): Target | null {
  const parts = split(url);
  if (parts === null) return null;
  const { host, path } = parts;
  if (host === 'github.com') {
    const [owner, name, what, n] = path.split('/');
    const at = number(n);
    if (!owner || !name || at === null) return null;
    if (what === 'pull') return { kind: 'pull', platform: 'github', repo: `${owner}/${name}`, number: at };
    if (what === 'issues') return { kind: 'issue', platform: 'github', repo: `${owner}/${name}`, number: at };
    return null;
  }
  for (const [marker, kind] of [
    ['/-/merge_requests/', 'pull'],
    ['/-/issues/', 'issue'],
  ] as const) {
    const index = path.indexOf(marker);
    if (index < 0) continue;
    const project = path.slice(0, index);
    const at = number(path.slice(index + marker.length).split('/')[0]);
    if (project === '' || at === null) return null;
    return { kind, platform: 'gitlab', repo: project, number: at };
  }
  return null;
}

/** The target in words: `pull request docspec/app #7`. */
export function targetText(target: Target): string {
  const what =
    target.kind === 'issue' ? 'issue' : target.platform === 'gitlab' ? 'merge request' : 'pull request';
  return `${what} ${target.repo} #${target.number}`;
}

/** What `start` needs `url` to name. */
export function needs(start: Start): Target['kind'] {
  return start === 'plan' ? 'issue' : 'pull';
}

export type Check = { ok: string } | { problem: string } | null;

/** Whether `url` fits `start`: what it names, or why not; nothing while
 * the field is empty. */
export function checkUrl(start: Start, url: string): Check {
  if (url.trim() === '') return null;
  const target = parseTarget(url);
  const wanted = needs(start);
  if (target === null) {
    return {
      problem:
        wanted === 'issue'
          ? 'Not a GitHub or GitLab issue URL.'
          : 'Not a GitHub pull request or GitLab merge request URL.',
    };
  }
  if (target.kind !== wanted) {
    return wanted === 'issue'
      ? { problem: `This is a ${target.platform === 'gitlab' ? 'merge request' : 'pull request'}. A plan needs an issue URL.` }
      : { problem: `This is an issue. ${start === 'review' ? 'A review' : 'Addressing feedback'} needs a pull request URL.` };
  }
  return { ok: targetText(target) };
}
