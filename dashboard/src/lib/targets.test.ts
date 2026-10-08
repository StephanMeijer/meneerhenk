import { describe, expect, it } from 'vitest';
import { checkUrl, parseTarget, targetText } from './targets';

describe('parseTarget', () => {
  it('reads GitHub pull requests and issues as Henk does', () => {
    expect(parseTarget('https://github.com/docspec/app/pull/7')).toEqual({
      kind: 'pull', platform: 'github', repo: 'docspec/app', number: 7,
    });
    expect(parseTarget(' https://github.com/docspec/app/pull/7/files/ ')).toMatchObject({ kind: 'pull', number: 7 });
    expect(parseTarget('http://github.com/docspec/app/issues/52')).toMatchObject({ kind: 'issue', number: 52 });
  });

  it('reads GitLab merge requests and issues on any host', () => {
    expect(parseTarget('https://gitlab.com/9xxlab/tools/cli/-/merge_requests/5')).toEqual({
      kind: 'pull', platform: 'gitlab', repo: '9xxlab/tools/cli', number: 5,
    });
    expect(parseTarget('https://git.example.org/ops/infra/-/issues/13/')).toMatchObject({
      kind: 'issue', platform: 'gitlab', repo: 'ops/infra', number: 13,
    });
  });

  it('turns down what only looks right', () => {
    for (const url of [
      '',
      'github.com/docspec/app/pull/7',
      'ftp://github.com/docspec/app/pull/7',
      'https://github.com',
      'https://github.com/docspec/app/pulls/7',
      'https://github.com/docspec/app/pull/x',
      'https://github.com/docspec/app/pull/-7',
      'https://github.com/docspec/app/pull/',
      'https://github.com/docspec/app',
      'https://github.com.evil.example/docspec/app/pull/7',
      'https://gitlab.com/-/merge_requests/5',
      'https://gitlab.com/ops/infra/merge_requests/5',
    ]) {
      expect(parseTarget(url), url).toBeNull();
    }
  });

  it('says what a target is', () => {
    expect(targetText({ kind: 'pull', platform: 'github', repo: 'o/r', number: 7 })).toBe('pull request o/r #7');
    expect(targetText({ kind: 'pull', platform: 'gitlab', repo: 'g/p', number: 5 })).toBe('merge request g/p #5');
    expect(targetText({ kind: 'issue', platform: 'github', repo: 'o/r', number: 9 })).toBe('issue o/r #9');
  });
});

describe('checkUrl', () => {
  it('says nothing about an empty field', () => {
    expect(checkUrl('review', '  ')).toBeNull();
  });

  it('names the target when it fits', () => {
    expect(checkUrl('review', 'https://github.com/docspec/app/pull/7')).toEqual({ ok: 'pull request docspec/app #7' });
    expect(checkUrl('address', 'https://gitlab.com/g/p/-/merge_requests/5')).toEqual({ ok: 'merge request g/p #5' });
    expect(checkUrl('plan', 'https://github.com/docspec/api/issues/52')).toEqual({ ok: 'issue docspec/api #52' });
  });

  it('says why when it does not', () => {
    expect(checkUrl('plan', 'https://github.com/docspec/api/pull/52')).toEqual({
      problem: 'This is a pull request. A plan needs an issue URL.',
    });
    expect(checkUrl('review', 'https://github.com/docspec/api/issues/52')).toEqual({
      problem: 'This is an issue. A review needs a pull request URL.',
    });
    expect(checkUrl('address', 'https://github.com/docspec/api/issues/52')).toEqual({
      problem: 'This is an issue. Addressing feedback needs a pull request URL.',
    });
    expect(checkUrl('review', 'nope')).toEqual({ problem: 'Not a GitHub pull request or GitLab merge request URL.' });
    expect(checkUrl('plan', 'nope')).toEqual({ problem: 'Not a GitHub or GitLab issue URL.' });
  });
});
