import { describe, expect, it } from 'vitest';
import { lookOf, statusLabel, VOCABULARIES, type Vocabulary } from './status';

describe('status', () => {
  it('gives every word of every vocabulary a tone and an icon', () => {
    for (const [name, table] of Object.entries(VOCABULARIES)) {
      for (const word of Object.keys(table)) {
        const look = lookOf(word, name as Vocabulary);
        expect(look.tone, `${name}/${word}`).toBeTruthy();
        expect(look.icon, `${name}/${word}`).toBeTruthy();
        expect(look.label).toBe(statusLabel(word));
      }
    }
  });

  it('says each status the way the design does', () => {
    expect(lookOf('running')).toEqual({ tone: 'accent', icon: 'spinner', label: 'running' });
    expect(lookOf('did_not_finish')).toEqual({ tone: 'drop', icon: 'broken', label: 'did not finish' });
    expect(lookOf('superseded').icon).toBe('minus');
    expect(lookOf('same_as', 'verdict')).toEqual({ tone: 'neutral', icon: 'link', label: 'same as' });
    expect(lookOf('withdrawn', 'action').icon).toBe('undo');
    expect(lookOf('refused_scope', 'outcome')).toEqual({ tone: 'refused', icon: 'shield', label: 'refused scope' });
  });

  it('reads the same word by what it is about', () => {
    expect(lookOf('rejected', 'verdict').tone).toBe('fail');
    expect(lookOf('cancelled', 'outcome').icon).toBe('slash');
  });

  it('shows a word it does not know as it is, neutral', () => {
    expect(lookOf('half_done')).toEqual({ tone: 'neutral', icon: 'circle', label: 'half done' });
    expect(lookOf('constructor')).toEqual({ tone: 'neutral', icon: 'circle', label: 'constructor' });
    expect(lookOf('toString', 'verdict').tone).toBe('neutral');
  });
});
