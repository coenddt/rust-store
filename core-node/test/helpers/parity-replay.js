'use strict';

/**
 * core-node parity commands 段的命令回放器（D4-4 零语义拆分）。
 *
 * 自 test/parity.test.js 原样下沉：`{{step.N._id}}` 占位符替换、写路径六种 kind 的
 * 宿主模拟回执行、exists 用例比较结构、只读/单命令与写路径分派表、按 kind 分派入口；
 * 语句与注释逐字未改。
 *
 * 放在 test/helpers 下（与 smoke-fixtures.js 同惯例）：package.json 的 test 脚本
 * 显式列出用例文件，helpers 不参与 node --test 收集，不产生计数漂移。
 */

const { has, planInsertShape } = require('./parity-harness');

/** 把命令里 `{{step.<N>._id}}` 占位符替换为第 N 步模拟执行的 `_id` */
function substituteStepPlaceholders(value, resolved) {
  if (typeof value === 'string') {
    const m = value.match(/^\{\{step\.(\d+)\._id\}\}$/);
    if (m && resolved[Number(m[1])] !== undefined) return resolved[Number(m[1])];
    return value;
  }
  if (Array.isArray(value)) return value.map((v) => substituteStepPlaceholders(v, resolved));
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = substituteStepPlaceholders(v, resolved);
    return out;
  }
  return value;
}

/** 写路径公共环境：ctx / now / 回喂后的 updatedDoc */
function writeEnv(fx) {
  return { ctx: fx.context ?? null, now: fx.now ?? 0, updated: fx.updatedDoc ?? null };
}

/** 宿主回喂结果（无 updatedDoc ⇒ null） */
function returnsOf(reg, fx, env) {
  return env.updated == null ? null : reg.applyWriteDefaults(fx.model, env.updated);
}

/** update：creator 受限写入 ⇒ core 先返回探针（宿主回喂后二次规划），黄金基准即 needsProbe 信封 */
function replayUpdate(reg, fx, env) {
  const out = reg.planUpdate(
    fx.model || '', fx.condition ?? null, fx.data ?? null, fx.options ?? null, env.now, env.ctx,
  );
  if (out.needsProbe) return out;
  return { commands: [out.command], returns: returnsOf(reg, fx, env) };
}

function replayUpdateMany(reg, fx, env) {
  const out = reg.planUpdateMany(fx.model || '', fx.condition ?? null, fx.data ?? null, env.now, env.ctx);
  return { command: out.command, returns: { modifiedCount: fx.modifiedCount ?? 0 } };
}

/** remove 的归档命令段：findCommand 存在才归档，docs 非空才追加 archive 命令并计 archivedCount */
function archiveCommands(reg, fx, plan, env) {
  const commands = [];
  let archivedCount = 0;
  if (plan.findCommand != null) {
    commands.push(plan.findCommand);
    const docs = fx.docs || [];
    if (docs.length) {
      commands.push(reg.planArchiveDocs(fx.model || '', docs, env.now).command);
      archivedCount = docs.length;
    }
  }
  return { commands, archivedCount };
}

/** remove：creator 受限删除 ⇒ 先返回探针（同上） */
function replayRemove(reg, fx, env) {
  const plan = reg.planRemove(fx.model || '', fx.condition ?? null, env.ctx);
  if (plan.needsProbe) return plan;
  const { commands, archivedCount } = archiveCommands(reg, fx, plan, env);
  commands.push(plan.deleteCommand);
  return { commands, returns: { deletedCount: fx.deletedCount ?? 0, archivedCount } };
}

function replayUpsert(reg, fx, env) {
  const out = reg.planUpsert(
    fx.model || '',
    fx.condition ?? null,
    fx.data ?? null,
    fx.options ?? null,
    env.now,
    (fx.newIds || [''])[0] || '',
    env.ctx,
  );
  return { command: out.command, returns: returnsOf(reg, fx, env) };
}

/** 单步 mutation 命令的模拟结果（insertOne → doc；findOneAndUpdate → 回喂文档） */
function mutationStepResult(command, env) {
  if (command.kind === 'insertOne') return command.doc ?? null;
  if (command.kind === 'findOneAndUpdate') return env.updated;
  throw new Error(`未支持的 mutation 步骤命令: ${command.kind}`);
}

function replayMutation(reg, fx, env) {
  const plan = reg.planMutation(fx.model || '', fx.data ?? null, env.now, fx.newIds || [], env.ctx);
  const resolved = [];
  const commands = [];
  let rootResult;
  for (const step of plan.steps) {
    const command = substituteStepPlaceholders(step.command, resolved);
    const result = mutationStepResult(command, env);
    if (rootResult === undefined) rootResult = result;
    resolved.push(result ? result._id ?? null : null);
    commands.push(command);
  }
  const returns = rootResult == null ? null : reg.applyWriteDefaults(fx.model, rootResult);
  return { commands, returns };
}

/** 写路径分派表：kind → 比较结构（未知 kind 返回 null，表示非 write 用例） */
const WRITE_REPLAYERS = {
  insert_many: (reg, fx, env) => reg.planInsertMany(fx.model || '', fx.docs || [], env.now, fx.newIds || [], env.ctx),
  update: replayUpdate,
  update_many: replayUpdateMany,
  remove: replayRemove,
  upsert: replayUpsert,
  mutation: replayMutation,
};

/** 写路径统一回放：Host 模拟执行命令并回喂结果（对齐 parity_write.rs） */
function replayWriteCase(reg, fx) {
  const replayer = WRITE_REPLAYERS[fx.kind];
  return replayer ? replayer(reg, fx, writeEnv(fx)) : null;
}

/** exists 用例：命令 + 「命中/未命中」与单元素集合视图 */
function planExistsCase(reg, fx) {
  const command = reg.planExists(fx.model || '', fx.condition ?? {});
  return {
    command,
    found: fx.foundDoc != null,
    collections: [command.collection ?? null],
  };
}

/** 只读/单命令分派表（写路径用例不在表内，由 replayWriteCase 兜住） */
const COMMAND_PLANNERS = {
  query: (reg, fx, ctx) => reg.planQuery(fx.gql || '', fx.params || {}, ctx),
  query_with_count: (reg, fx, ctx) => reg.planQueryWithCount(fx.gql || '', fx.params || {}, ctx, fx.total ?? 0),
  resolve_page: (reg, fx) => reg.resolvePage(fx.gql || '', fx.params || {}),
  restore_sort_order: (reg, fx) => reg.restoreSortOrder(fx.items || [], fx.ids || [], fx.sort ?? null),
  insert: (reg, fx, ctx) => planInsertShape(reg, fx, ctx),
  exists: planExistsCase,
  count: (reg, fx) => ({ command: reg.planCount(fx.model || '', fx.filter ?? null) }),
};

/** 按 `kind` 分派，产出与 parity_commands.rs 完全相同的比较结构 */
function runCommandCase(reg, fx) {
  const planner = COMMAND_PLANNERS[fx.kind];
  if (planner) return planner(reg, fx, fx.context ?? null);
  // 写路径用例走 replayWriteCase（对齐 parity_write.rs 的 Host 模拟执行）
  return replayWriteCase(reg, fx);
}

module.exports = { runCommandCase };
