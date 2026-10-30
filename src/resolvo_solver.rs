//! resolvo 适配器：把 xrpkg 的 Capability/包数据接入 resolvo。
//!
//! 手写玩具求解器保留在 `solver` 模块中作为对照；本项目主流程调用本模块。

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use resolvo::conflict::{Conflict, ConflictEdge, ConflictNode};
use resolvo::utils::{Pool, VersionSet as ResolvoVersionSet};
use resolvo::{
    Candidates, Condition, ConditionId, ConditionalRequirement, Dependencies, DependencyProvider,
    HintDependenciesAvailable, Interner, KnownDependencies, NameId, Problem, Requirement,
    SolvableId, Solver, SolverCache, StringId, UnsolvableOrCancelled, VersionSetId,
    VersionSetUnionId,
};

use crate::capability::{Capability, CapabilityOp, Version};
use crate::solver::{Package, Resolution, SelectedPackage};
use crate::state::InstalledPackage;

/// resolvo 的版本集：只保存操作符和边界版本，包名由 Pool 单独管理。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct VersionConstraint {
    op: Option<CapabilityOp>,
    version: Option<Version>,
}

impl VersionConstraint {
    /// 从 Capability 中剥离包名，得到 resolvo 需要的版本集合。
    fn from_capability(capability: &Capability) -> Self {
        Self {
            op: capability.op,
            version: capability.version.clone(),
        }
    }

    /// 判断具体版本是否属于该版本集合。
    fn matches(&self, version: &Version) -> bool {
        match (self.op, self.version.as_ref()) {
            (None, None) => true,
            (Some(op), Some(bound)) => match op {
                CapabilityOp::GreaterThanOrEqual => version >= bound,
                CapabilityOp::LessThan => version < bound,
                CapabilityOp::Equal => version == bound,
            },
            _ => false,
        }
    }
}

impl fmt::Display for VersionConstraint {
    /// 只输出版本约束部分，不输出包名；裸包名输出空串。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.op, self.version.as_ref()) {
            (Some(op), Some(version)) => write!(formatter, "{op} {version}"),
            _ => Ok(()),
        }
    }
}

impl ResolvoVersionSet for VersionConstraint {
    type V = Version;
}

/// 把已安装但仓库配置中不存在的包补成“无依赖候选”。
///
/// 本地 `--build` 产生的包没有写回 packages.toml；如果求解器仍要求
/// 所有已安装包都是候选，下一次安装会因为找不到这些包而失败。
fn with_installed_packages(packages: &[Package], installed: &[InstalledPackage]) -> Vec<Package> {
    let mut merged = packages.to_vec();
    for installed_package in installed {
        if !merged
            .iter()
            .any(|package| package.name == installed_package.name)
        {
            merged.push(Package::new(
                installed_package.name.clone(),
                installed_package.version.clone(),
                Vec::new(),
            ));
        }
    }
    merged
}

/// 把 xrpkg 包数据转换成 resolvo 查询的适配器。
struct ResolvoProvider {
    pool: Pool<VersionConstraint>,
    candidates: HashMap<NameId, Vec<SolvableId>>,
    package_index_by_solvable: HashMap<SolvableId, usize>,
    dependencies: Vec<Vec<VersionSetId>>,
    installed: HashMap<String, Version>,
}

impl ResolvoProvider {
    /// 为每个包版本建立 solvable，并预先把所有依赖转换成 version set。
    fn new(packages: &[Package], installed: &[InstalledPackage]) -> Self {
        let pool = Pool::new();
        let mut candidates: HashMap<NameId, Vec<SolvableId>> = HashMap::new();
        let mut package_index_by_solvable = HashMap::new();
        let installed = installed
            .iter()
            .map(|package| (package.name.clone(), package.version.clone()))
            .collect();

        for (index, package) in packages.iter().enumerate() {
            let name = pool.intern_package_name(package.name.as_str());
            let solvable = pool.intern_solvable(name, package.version.clone());
            package_index_by_solvable.insert(solvable, index);
            candidates.entry(name).or_default().push(solvable);
        }

        let dependencies = packages
            .iter()
            .map(|package| {
                package
                    .requires
                    .iter()
                    .map(|capability| {
                        let name = pool.intern_package_name(capability.name.as_str());
                        pool.intern_version_set(
                            name,
                            VersionConstraint::from_capability(capability),
                        )
                    })
                    .collect()
            })
            .collect();

        Self {
            pool,
            candidates,
            package_index_by_solvable,
            dependencies,
            installed,
        }
    }

    /// 把一条 Capability 注册为 resolvo 的 VersionSetId。
    fn intern_capability(&self, capability: &Capability) -> VersionSetId {
        let name = self.pool.intern_package_name(capability.name.as_str());
        self.pool
            .intern_version_set(name, VersionConstraint::from_capability(capability))
    }
}

impl Interner for ResolvoProvider {
    type NameId = NameId;
    type SolvableId = SolvableId;

    /// 在错误树中显示“包名 版本”。
    fn display_solvable(&self, solvable: SolvableId) -> impl fmt::Display + '_ {
        let solvable = self.pool.resolve_solvable(solvable);
        format!(
            "{} {}",
            self.pool.resolve_package_name(solvable.name),
            solvable.record
        )
    }

    /// 合并显示同一包的多个候选版本，避免错误树出现“app app 1.0”。
    fn display_merged_solvables(&self, solvables: &[SolvableId]) -> impl fmt::Display + '_ {
        if solvables.is_empty() {
            return String::new();
        }

        let name = self.pool.resolve_solvable(solvables[0]).name;
        let mut versions = solvables
            .iter()
            .map(|solvable| self.pool.resolve_solvable(*solvable).record.to_string())
            .collect::<Vec<_>>();
        versions.sort();
        versions.dedup();

        format!(
            "{} {}",
            self.pool.resolve_package_name(name),
            versions.join(" | ")
        )
    }

    /// 显示内部包名。
    fn display_name(&self, name: NameId) -> impl fmt::Display + '_ {
        self.pool.resolve_package_name(name).clone()
    }

    /// 显示版本集合，例如 `>= 2.0`。
    fn display_version_set(&self, version_set: VersionSetId) -> impl fmt::Display + '_ {
        self.pool.resolve_version_set(version_set).clone()
    }

    /// 显示 interned 字符串。
    fn display_string(&self, string_id: StringId) -> impl fmt::Display + '_ {
        self.pool.resolve_string(string_id).to_owned()
    }

    /// 返回版本集合所属包名。
    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }

    /// 返回 solvable 所属包名。
    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }

    /// 返回并集里的各个版本集合；本项目阶段 2 没有使用并集。
    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(version_set_union)
    }

    /// 条件依赖在本阶段未使用；resolvo 不应请求该转换。
    fn resolve_condition(&self, _condition: ConditionId) -> Condition {
        unreachable!("阶段 2 不使用条件依赖")
    }
}

impl DependencyProvider for ResolvoProvider {
    /// 按 version set 过滤 resolvo 给出的候选 solvable。
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let constraint = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|solvable| {
                let version = &self.pool.resolve_solvable(*solvable).record;
                constraint.matches(version) != inverse
            })
            .collect()
    }

    /// 返回某个包名下的全部候选 solvable。
    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.candidates.get(&name).map(|candidates| {
            let mut result = Candidates {
                candidates: candidates.clone(),
                hint_dependencies_available: HintDependenciesAvailable::All,
                ..Candidates::default()
            };
            result.favored = self
                .installed
                .get(self.pool.resolve_package_name(name))
                .and_then(|installed_version| {
                    result.candidates.iter().copied().find(|solvable| {
                        &self.pool.resolve_solvable(*solvable).record == installed_version
                    })
                });
            result
        })
    }

    /// 最高版本优先；失败时 resolvo 会自己尝试下一个候选。
    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        solvables.sort_by(|left, right| {
            let left = &self.pool.resolve_solvable(*left).record;
            let right = &self.pool.resolve_solvable(*right).record;
            right.cmp(left)
        });
    }

    /// 返回该 solvable 的全部依赖要求。
    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let Some(&index) = self.package_index_by_solvable.get(&solvable) else {
            let reason = self.pool.intern_string("无法定位包记录");
            return Dependencies::Unknown(reason);
        };

        let requirements = self.dependencies[index]
            .iter()
            .copied()
            .map(ConditionalRequirement::from)
            .collect();

        Dependencies::Known(KnownDependencies {
            requirements,
            constrains: Vec::new(),
        })
    }
}

/// 使用空安装状态求解指定根包；保留给阶段 2 的回归测试。
pub fn resolve(packages: &[Package], root: &str) -> Result<Resolution, String> {
    resolve_install(packages, root, &[])
}

/// 在“已安装包必须继续存在”的前提下，求解一次增量安装。
///
/// 已安装包会被加入根要求，同时其精确版本在 provider 中作为 favored 候选。
/// 这样默认保留现版本，但新依赖确实要求更高版本时，resolvo 仍可升级。
pub fn resolve_install(
    packages: &[Package],
    root: &str,
    installed: &[InstalledPackage],
) -> Result<Resolution, String> {
    if !packages.iter().any(|package| package.name == root) {
        return Err(format!("无解: 仓库中不存在包 `{root}`"));
    }
    let packages = with_installed_packages(packages, installed);
    let root_capability = Capability {
        name: root.to_string(),
        op: None,
        version: None,
    };
    resolve_with_capability(&packages, root, installed, &root_capability)
}

/// 求解指定包的最新版本，用于 install 时自动升级已安装的旧版本。
pub fn resolve_install_latest(
    packages: &[Package],
    root: &str,
    installed: &[InstalledPackage],
) -> Result<Resolution, String> {
    resolve_install_target(packages, root, installed, None)
}

/// 求解显式指定的包版本；target 为 `None` 时取最新版本。
pub fn resolve_install_version(
    packages: &[Package],
    root: &str,
    installed: &[InstalledPackage],
    target: Option<&Version>,
) -> Result<Resolution, String> {
    resolve_install_target(packages, root, installed, target)
}

fn resolve_install_target(
    packages: &[Package],
    root: &str,
    installed: &[InstalledPackage],
    target: Option<&Version>,
) -> Result<Resolution, String> {
    let packages = with_installed_packages(packages, installed);
    let version = match target {
        Some(version) => version.clone(),
        None => packages
            .iter()
            .filter(|package| package.name == root)
            .map(|package| package.version.clone())
            .max()
            .ok_or_else(|| format!("无解: 仓库中不存在包 `{root}`"))?,
    };
    let root_capability = Capability {
        name: root.to_string(),
        op: Some(crate::capability::CapabilityOp::Equal),
        version: Some(version),
    };
    resolve_with_capability(&packages, root, installed, &root_capability)
}

fn resolve_with_capability(
    packages: &[Package],
    root: &str,
    installed: &[InstalledPackage],
    root_capability: &Capability,
) -> Result<Resolution, String> {
    let provider = ResolvoProvider::new(packages, installed);
    let mut requirements = vec![ConditionalRequirement::from(
        provider.intern_capability(root_capability),
    )];
    for package in installed {
        if package.name == root {
            continue;
        }
        let capability = Capability {
            name: package.name.clone(),
            op: None,
            version: None,
        };
        requirements.push(provider.intern_capability(&capability).into());
    }

    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);

    match solver.solve(problem) {
        Ok(solved) => {
            let provider = solver.provider();
            let mut selected = BTreeMap::new();
            for solvable_id in solved {
                let Some(&index) = provider.package_index_by_solvable.get(&solvable_id) else {
                    continue;
                };
                let solvable = provider.pool.resolve_solvable(solvable_id);
                let name = provider.pool.resolve_package_name(solvable.name).clone();
                selected.insert(name, index);
            }
            Ok(Resolution::from_selected(packages, root, &selected))
        }
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            if let Some(message) = format_missing_candidate(packages, &conflict, &solver) {
                Err(message)
            } else {
                let details = conflict.display_user_friendly(&solver).to_string();
                Err(format_resolvo_conflict(&details))
            }
        }
        Err(UnsolvableOrCancelled::Cancelled(_)) => Err("无解: 求解过程被取消".to_string()),
    }
}

/// 求解一组外部依赖映射后的 Capability，用于兼容轨道安装前的依赖预检。
///
/// 返回的包列表按包名排序，仅用于展示和判断可满足性，不代表 native 安装顺序。
pub fn resolve_requirements(
    packages: &[Package],
    requirements: &[Capability],
    installed: &[InstalledPackage],
) -> Result<Vec<SelectedPackage>, String> {
    let packages = with_installed_packages(packages, installed);
    let provider = ResolvoProvider::new(&packages, installed);
    let mut root_requirements = requirements
        .iter()
        .map(|capability| ConditionalRequirement::from(provider.intern_capability(capability)))
        .collect::<Vec<_>>();

    for package in installed {
        if !requirements
            .iter()
            .any(|capability| capability.name == package.name)
        {
            let capability = Capability {
                name: package.name.clone(),
                op: None,
                version: None,
            };
            root_requirements.push(provider.intern_capability(&capability).into());
        }
    }

    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(root_requirements);

    match solver.solve(problem) {
        Ok(solved) => {
            let provider = solver.provider();
            let mut selected = solved
                .into_iter()
                .map(|solvable_id| {
                    let solvable = provider.pool.resolve_solvable(solvable_id);
                    let name = provider.pool.resolve_package_name(solvable.name).clone();
                    SelectedPackage {
                        name,
                        version: solvable.record.clone(),
                    }
                })
                .collect::<Vec<_>>();
            selected.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(selected)
        }
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            if let Some(message) = format_missing_candidate(&packages, &conflict, &solver) {
                Err(message)
            } else {
                let details = conflict.display_user_friendly(&solver).to_string();
                Err(format_resolvo_conflict(&details))
            }
        }
        Err(UnsolvableOrCancelled::Cancelled(_)) => Err("无解: 依赖预检过程被取消".to_string()),
    }
}

/// 从完整解中筛出需要写入数据库的变化：新包，以及版本发生变化的已安装包。
pub fn incremental_plan(
    resolution: &Resolution,
    installed: &[InstalledPackage],
) -> Vec<SelectedPackage> {
    resolution
        .packages()
        .iter()
        .filter(|package| {
            !installed.iter().any(|installed_package| {
                installed_package.name == package.name
                    && installed_package.version == package.version
            })
        })
        .cloned()
        .collect()
}

/// 如果冲突树里有“依赖存在但没有任何候选版本”的叶子，就用阶段 1 的句式报告。
///
/// 这样既能利用 resolvo 的冲突图，又能保留上一阶段“谁依赖什么、有哪些可用版本”的清晰度。
fn format_missing_candidate(
    packages: &[Package],
    conflict: &Conflict,
    solver: &Solver<ResolvoProvider>,
) -> Option<String> {
    let graph = conflict.graph(solver);
    let unresolved = graph.unresolved_node?;
    let provider = solver.provider();

    for edge in graph.graph.raw_edges() {
        if edge.target() != unresolved {
            continue;
        }

        let ConflictEdge::Requires(Requirement::Single(version_set)) = &edge.weight else {
            continue;
        };
        let Some(ConflictNode::Solvable(parent)) = graph.graph.node_weight(edge.source()) else {
            continue;
        };

        let parent_solvable = provider.pool.resolve_solvable(*parent);
        let requester = provider
            .pool
            .resolve_package_name(parent_solvable.name)
            .clone();
        let dependency_name = provider
            .pool
            .resolve_package_name(provider.pool.resolve_version_set_package_name(*version_set))
            .clone();
        let constraint = provider.pool.resolve_version_set(*version_set);
        let capability = Capability {
            name: dependency_name.clone(),
            op: constraint.op,
            version: constraint.version.clone(),
        };

        let mut versions = packages
            .iter()
            .filter(|package| package.name == capability.name)
            .map(|package| package.version.clone())
            .collect::<Vec<_>>();
        versions.sort();
        versions.dedup();

        if versions.is_empty() {
            return Some(format!(
                "无解: 包 `{requester}` 依赖 `{capability}`，但仓库中不存在 `{dependency_name}`"
            ));
        }

        let available = versions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "无解: 包 `{requester}` 依赖 `{capability}`，但仓库中没有满足该约束的版本；`{dependency_name}` 可用版本: {available}"
        ));
    }

    None
}

/// 把 resolvo 的冲突树包成与阶段 1 风格相近的中文错误。
fn format_resolvo_conflict(details: &str) -> String {
    if details.trim().is_empty() {
        "无解: resolvo 未找到满足全部依赖的版本组合".to_string()
    } else {
        format!("无解: resolvo 未找到满足全部依赖的版本组合\nresolvo 冲突详情:\n{details}")
    }
}
