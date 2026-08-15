<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Setup Wizard: 3 skippable steps, only
// when user table is empty; each has a CLI equivalent (ADR-0010).
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'

const { t } = useI18n()
const step = ref(0)
const steps = ['step1', 'step2', 'step3']
</script>

<template>
  <div class="wizard">
    <div class="brand">⬢ sisyphus</div>
    <h1>{{ t('setup.title') }}</h1>

    <div class="progress">
      <div v-for="(s, i) in steps" :key="s" class="pstep" :class="{ done: i < step, now: i === step }">
        <div class="dot">{{ i < step ? '✓' : i + 1 }}</div>
        <div class="plabel">{{ t(`setup.${s}`) }}</div>
      </div>
    </div>

    <div class="card body">
      <!-- step 1: admin -->
      <div v-if="step === 0">
        <h2>{{ t('setup.step1') }}</h2>
        <div class="form-row"><label>admin 用户名</label><input type="text" value="admin" /></div>
        <div class="form-row"><label>密码</label><input type="password" value="········" /></div>
        <div class="cli mono">CLI: sisyphus-server admin create --user admin</div>
      </div>

      <!-- step 2: agent -->
      <div v-else-if="step === 1">
        <h2>{{ t('setup.step2') }}</h2>
        <div class="mono cmd">
          sisyphus-agent --server https://ci.example.com \<br />
          &nbsp;&nbsp;--registration-code SISAR-9f2K-mQ7p-x7Qd
        </div>
        <div class="hint">在构建机上执行；注册码换取长期 token（sisa_）落盘</div>
        <div class="status ok">✓ 已检测到 build-linux-01 上线（1.0.3）</div>
      </div>

      <!-- step 3: project -->
      <div v-else>
        <h2>{{ t('setup.step3') }}</h2>
        <div class="form-row"><label>{{ t('common.name') }}</label><input type="text" placeholder="my-project" /></div>
        <div class="form-row"><label>{{ t('projects.scmType') }}</label><select><option>git</option><option>svn</option></select></div>
        <div class="form-row"><label>{{ t('projects.repoUrl') }}</label><input type="text" style="flex:1" placeholder="https://github.com/org/repo.git" /></div>
      </div>

      <div class="actions">
        <span class="hint">{{ t('setup.cliNote') }}</span>
        <div class="row">
          <button class="btn" @click="step = Math.min(step + 1, 2)">{{ t('setup.skip') }}</button>
          <button v-if="step < 2" class="btn primary" @click="step++">{{ t('setup.next') }}</button>
          <button v-else class="btn primary">{{ t('setup.done') }}</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.wizard { max-width: 560px; margin: 8vh auto 0; }
.brand { font-weight: 700; font-size: 18px; margin-bottom: 14px; }
.progress { display: flex; justify-content: space-between; margin-bottom: 18px; }
.pstep { display: flex; flex-direction: column; align-items: center; gap: 4px; flex: 1; position: relative; }
.pstep::before { content: ''; position: absolute; top: 12px; left: -50%; width: 100%; height: 2px; background: var(--line); }
.pstep:first-child::before { display: none; }
.pstep.done::before { background: var(--ok); }
.dot { width: 26px; height: 26px; border-radius: 50%; background: #fff; border: 2px solid var(--line); display: flex; align-items: center; justify-content: center; font-size: 13px; font-weight: 700; z-index: 1; }
.pstep.done .dot { background: var(--ok); border-color: var(--ok); color: #fff; }
.pstep.now .dot { border-color: var(--accent); color: var(--accent); }
.plabel { font-size: 12px; color: var(--ink-dim); }
.pstep.now .plabel { color: var(--accent); font-weight: 600; }
.form-row { display: flex; align-items: center; gap: 10px; margin-bottom: 10px; }
.form-row label { width: 110px; color: var(--ink-dim); font-size: 13px; }
.cli { margin-top: 14px; background: #f6f8fa; border: 1px dashed var(--line); border-radius: 6px; padding: 6px 10px; font-size: 11.5px; color: var(--ink-dim); }
.cmd { background: #0f172a; color: #d7e0ee; border-radius: 6px; padding: 12px 14px; font-size: 12.5px; line-height: 1.7; }
.hint { font-size: 12px; color: var(--ink-dim); }
.status { margin-top: 12px; font-size: 13px; }
.status.ok { color: var(--ok); }
.actions { display: flex; justify-content: space-between; align-items: center; margin-top: 18px; border-top: 1px solid var(--line); padding-top: 14px; }
</style>
