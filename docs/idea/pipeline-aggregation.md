# Pipeline 聚合式编排设计构想

## 核心思想

Pipeline 是一种聚合形式，而非单体脚本。每个步骤都可独立执行，实际构建运行的是编排好的 pipeline 变体。

## 层次结构

```
Pipeline (变体)
  └─ Sub-pipeline (逻辑阶段)
       └─ Step (原子可执行单元)
```

### Step（原子单元）
- 最小粒度的可执行单元
- 独立可执行、可测试、可复用
- 定义自己的输入/输出、超时、重试策略
- 同一个 step 可以出现在多个 sub-pipeline 中

### Sub-pipeline（逻辑阶段）
- 按业务语义组织 step 的聚合体
- 例如："编译"、"测试"、"部署"、"通知"
- 内部 step 之间可以有串行/并行/条件依赖关系
- sub-pipeline 本身也可以嵌套组合

### Pipeline variant（变体）
- 一次实际构建执行的编排实例
- 从可用的 sub-pipeline 中选取并排序
- 可以叠加条件：when 表达式、fail-fast 策略
- 同一套 step/sub-pipeline 可以编排出多个变体
  - 例如：PR 构建（编译+单测）vs 发布构建（编译+全量测试+部署）

## 配置形式设想

```yaml
# step 定义
steps:
  checkout:
    kind: shell
    script: "git clone {{ .repo }} ."
  build:
    kind: shell
    script: "cargo build --release"
  unit-test:
    kind: shell
    script: "cargo test --lib"
  deploy:
    kind: shell
    script: "helm upgrade --install ..."

# sub-pipeline 组合
sub-pipelines:
  compile:
    steps: [checkout, build]
  test:
    steps: [unit-test]
    needs: compile
  release:
    steps: [deploy]
    needs: [compile, test]

# pipeline 变体
pipelines:
  pr-check:
    stages: [compile, test]
    fail-fast: true
  release:
    stages: [compile, test, release]
    fail-fast: false
```

## 待思考

- [ ] step 的幂等性保证
- [ ] sub-pipeline 之间的产物传递（artifact flow）
- [ ] 条件执行的表达式语言（when 求值）
- [ ] step 版本管理与向后兼容
- [ ] 与现有 trigger / agent 模型的衔接
