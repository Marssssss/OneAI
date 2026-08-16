// WorkspaceStore — unit tests for the web-local workspace list + current
// selection (localStorage-backed). The grouping logic in SessionList is
// driven off `workspace` on SessionInfo; this guards the path→alias resolver
// + upsert/remove/setCurrent round-trips the store is built on.

import { beforeEach, describe, expect, it } from 'vitest'
import {
  workspaceStore,
  workspaceLabel,
} from './workspaceStore'

describe('workspaceStore', () => {
  beforeEach(() => {
    localStorage.clear()
    // The singleton only reads localStorage in its constructor; re-read so
    // each test starts from a clean slate.
    workspaceStore.__resetForTests()
  })

  it('derives an alias from the path basename', () => {
    expect(workspaceLabel('/Users/me/projects/foo')).toBe('foo')
    expect(workspaceLabel('/Users/me/projects/foo/')).toBe('foo')
    expect(workspaceLabel('/')).toBe('/')
  })

  it('upserts a workspace and resolves its alias', () => {
    const p = workspaceStore.upsert('/x/y/my-project', 'My Project')
    expect(p).toBe('/x/y/my-project')
    expect(workspaceStore.labelFor('/x/y/my-project')).toBe('My Project')
  })

  it('defaults the alias to the basename when none given', () => {
    workspaceStore.upsert('/a/b/coders')
    expect(workspaceStore.labelFor('/a/b/coders')).toBe('coders')
  })

  it('updates the alias on a second upsert of the same path (no dup)', () => {
    workspaceStore.upsert('/p/alpha', 'old')
    workspaceStore.upsert('/p/alpha', 'new')
    const list = workspaceStore.list_all()
    expect(list.filter((w) => w.path === '/p/alpha')).toHaveLength(1)
    expect(workspaceStore.labelFor('/p/alpha')).toBe('new')
  })

  it('sets and clears the current selection', () => {
    workspaceStore.setCurrent('/srv/app')
    expect(workspaceStore.getSnapshot().current).toBe('/srv/app')
    workspaceStore.setCurrent(null)
    expect(workspaceStore.getSnapshot().current).toBeNull()
  })

  it('removes a workspace and clears current if it matched', () => {
    workspaceStore.upsert('/gone', 'Gone')
    workspaceStore.setCurrent('/gone')
    workspaceStore.remove('/gone')
    expect(workspaceStore.list_all().find((w) => w.path === '/gone')).toBeUndefined()
    expect(workspaceStore.getSnapshot().current).toBeNull()
  })
})
