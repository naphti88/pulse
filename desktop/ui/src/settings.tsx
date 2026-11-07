import { useEffect, useState } from 'react'

import { getSettings, saveSettings } from './api'
import { Glyph, StateMark } from './glyphs'
import type { Appearance, Limits, Rule, Settings } from './types'

const STATES = [
  ['blocked', 'Blocked'],
  ['waiting', 'Question · open points'],
  ['review', 'Result'],
  ['commands', 'Commands to check'],
] as const

// when a command is worth a look, in minutes
const LIMITS: [keyof Limits, string][] = [
  ['forgottenMin', 'Forgotten: background, session quiet for'],
  ['longMin', 'Running long: background, running for'],
  ['stuckMin', 'Stuck: foreground, session waiting for'],
  ['hotMin', 'Running hot: almost a whole core for'],
]

const APPEARANCES: [Appearance, string][] = [
  ['system', 'System'],
  ['light', 'Light'],
  ['dark', 'Dark'],
]

const MAX_HOURS = 720

const hhmm = (minutes: number) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`

// The notifications on the Mac: per state at once and while it lies, the
// hours of the reminders, the morning summary. Each change is saved and in
// force at once; the Rust side checks it and hands back what it keeps.
export function SettingsView({ onDone }: { onDone: () => void }) {
  const [settings, setSettings] = useState<Settings | null>(null)
  const [problem, setProblem] = useState<string | null>(null)
  const [hour, setHour] = useState('')

  useEffect(() => {
    getSettings().then(
      view => {
        setSettings(view.settings)
        setProblem(view.problem ? `The settings could not be read, the defaults apply: ${view.problem}` : null)
      },
      e => setProblem(String(e)),
    )
  }, [])

  if (!settings) return <div className="empty">{problem ?? 'loading …'}</div>

  const save = (next: Settings) => {
    setSettings(next)
    saveSettings(next).then(
      kept => {
        setSettings(kept)
        setProblem(null)
      },
      e => {
        setProblem(String(e))
        void getSettings().then(view => setSettings(view.settings))
      },
    )
  }
  const setRule = (state: (typeof STATES)[number][0], key: keyof Rule, value: boolean) =>
    save({ ...settings, [state]: { ...settings[state], [key]: value } })

  const addHour = () => {
    const n = Number(hour)
    if (!Number.isInteger(n) || n < 1 || n > MAX_HOURS) {
      setProblem(`Reminders: a whole number from 1 to ${MAX_HOURS} hours`)
      return
    }
    setHour('')
    save({ ...settings, remindHours: [...settings.remindHours, n] })
  }

  return (
    <div className="settings">
      <div className="st-head">
        <b>Settings</b>
        <button className="link" onClick={onDone}>
          Done
        </button>
      </div>
      {problem && <p className="st-problem">{problem}</p>}

      <div className="st-row">
        <span className="st-label">Appearance</span>
        <div className="seg">
          {APPEARANCES.map(([id, label]) => (
            <button key={id} className={settings.appearance === id ? 'on' : ''} onClick={() => save({ ...settings, appearance: id })}>
              {label}
            </button>
          ))}
        </div>
      </div>

      <span className="st-label st-section">Notifications</span>

      <table className="st-rules">
        <thead>
          <tr>
            <th />
            <th>at once</th>
            <th>remind</th>
          </tr>
        </thead>
        <tbody>
          {STATES.map(([state, label]) => (
            <tr key={state}>
              <td>
                <span className="st-state">
                  {state === 'commands' ? <Glyph name="commands" size={14} className="s-commands" /> : <StateMark state={state} size={14} />}
                  {label}
                </span>
              </td>
              <td>
                <input type="checkbox" checked={settings[state].atOnce} onChange={e => setRule(state, 'atOnce', e.target.checked)} />
              </td>
              <td>
                <input type="checkbox" checked={settings[state].remind} onChange={e => setRule(state, 'remind', e.target.checked)} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="st-note">At once: when an item newly comes up. Remind: while it stays open. A command you mark as known reports again at its next reminder step, or when it is worth a look for another reason.</p>

      <div className="st-block">
        <span className="st-label">Remind after</span>
        <div className="st-hours">
          {settings.remindHours.map(h => (
            <span key={h} className="chip">
              {h} h
              <button title="Remove" onClick={() => save({ ...settings, remindHours: settings.remindHours.filter(x => x !== h) })}>
                ×
              </button>
            </span>
          ))}
          <form
            className="st-add"
            onSubmit={e => {
              e.preventDefault()
              addHour()
            }}
          >
            <input type="number" min={1} max={MAX_HOURS} placeholder="hrs" value={hour} onChange={e => setHour(e.target.value)} />
            <button className="link" type="submit" disabled={hour === ''}>
              add
            </button>
          </form>
        </div>
        <label className="st-check">
          <input type="checkbox" checked={settings.remindDaily} onChange={e => save({ ...settings, remindDaily: e.target.checked })} />
          then daily
        </label>
      </div>

      <div className="st-block">
        <span className="st-label">Check commands when</span>
        {LIMITS.map(([key, label]) => (
          <label key={key} className="st-limit">
            <span>{label}</span>
            <input
              type="number"
              min={1}
              max={MAX_HOURS * 60}
              defaultValue={settings.limits[key]}
              key={settings.limits[key]}
              onBlur={e => {
                const n = Number(e.target.value)
                if (n === settings.limits[key]) return
                if (!Number.isInteger(n) || n < 1 || n > MAX_HOURS * 60) {
                  setProblem(`Commands: a whole number from 1 to ${MAX_HOURS * 60} minutes`)
                  e.target.value = String(settings.limits[key])
                  return
                }
                save({ ...settings, limits: { ...settings.limits, [key]: n } })
              }}
              onKeyDown={e => {
                if (e.key === 'Enter') e.currentTarget.blur()
              }}
            />
            <span className="st-unit">min</span>
          </label>
        ))}
      </div>

      <div className="st-block">
        <label className="st-check">
          <input type="checkbox" checked={settings.summary} onChange={e => save({ ...settings, summary: e.target.checked })} />
          Morning summary at
        </label>
        <input
          type="time"
          className="st-time"
          value={hhmm(settings.summaryAt)}
          disabled={!settings.summary}
          onChange={e => {
            const [h, m] = e.target.value.split(':').map(Number)
            if (h !== undefined && m !== undefined && !Number.isNaN(h) && !Number.isNaN(m)) save({ ...settings, summaryAt: h * 60 + m })
          }}
        />
        <label className="st-check">
          <input type="checkbox" checked={settings.parkedWeekly} disabled={!settings.summary} onChange={e => save({ ...settings, parkedWeekly: e.target.checked })} />
          on Mondays with the parked topics
        </label>
      </div>
      <p className="st-note">Quiet hours are left to the system's Focus or Do not disturb. The icon and the panel always show everything; parked topics count nowhere.</p>
    </div>
  )
}
