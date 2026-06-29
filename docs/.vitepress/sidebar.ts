import fs from "node:fs";
import path from "node:path";

/** 默认展开的模块（在侧边栏中 collapsed: false）。 */
const EXPANDED = new Set(["architecture", "routing"]);

/** 将目录名首字母大写。 */
function capitalize(name: string): string {
	return name.charAt(0).toUpperCase() + name.slice(1);
}

/**
 * 从 requirements.md 或 spec.md 的一级标题提取模块显示名。
 * 优先读 requirements.md，取第一个 `# ` 开头的 H1 内容作为标签。
 * 若不存在 requirements.md，读 spec.md，取 H1 并去除尾部 " — 规格" 后缀。
 * 若两者都不存在，返回 null（该目录不是模块，作为容器目录处理）。
 */
function extractLabel(dir: string): string | null {
	const reqPath = path.join(dir, "requirements.md");
	if (fs.existsSync(reqPath)) {
		const content = fs.readFileSync(reqPath, "utf-8");
		const match = content.match(/^#\s+(.+)/m);
		if (match) return match[1].trim();
	}

	const specPath = path.join(dir, "spec.md");
	if (fs.existsSync(specPath)) {
		const content = fs.readFileSync(specPath, "utf-8");
		const match = content.match(/^#\s+(.+)/m);
		if (match) {
			return match[1].trim().replace(/ — 规格$/, "");
		}
	}

	return null;
}

/**
 * 判断一个目录是否为模块目录（包含 requirements.md 或 spec.md）。
 */
function isModuleDir(dir: string): boolean {
	return (
		fs.existsSync(path.join(dir, "requirements.md")) ||
		fs.existsSync(path.join(dir, "spec.md"))
	);
}

/**
 * 为单个模块目录生成 sidebar 子项（需求/规格链接）。
 */
function buildModuleItems(dir: string, relPath: string): { text: string; link: string }[] {
	const items: { text: string; link: string }[] = [];
	const hasReq = fs.existsSync(path.join(dir, "requirements.md"));
	const hasSpec = fs.existsSync(path.join(dir, "spec.md"));
	if (hasReq) items.push({ text: "需求", link: `/${relPath}/requirements` });
	if (hasSpec) items.push({ text: "规格", link: `/${relPath}/spec` });
	return items;
}

/**
 * 递归扫描 specDir 下的目录，生成 VitePress sidebar 数组。
 *
 * 扫描规则：
 * - 一级子目录中，如果自身是模块（有 requirements.md 或 spec.md），直接生成 sidebar 条目。
 * - 如果一级子目录自身不是模块，则作为容器目录，递归扫描其子目录中的模块，生成嵌套分组。
 * - 容器目录的显示名使用其目录名的首字母大写形式。
 */
export function generateSpecSidebar(specDir: string) {
	const items: any[] = [];

	const entries = fs.readdirSync(specDir, { withFileTypes: true });
	for (const entry of entries) {
		if (!entry.isDirectory()) continue;
		const name = entry.name;
		const dir = path.join(specDir, name);

		if (isModuleDir(dir)) {
			// 一级模块
			const label = extractLabel(dir);
			if (!label) continue;
			const sub = buildModuleItems(dir, `spec/${name}`);
			items.push({
				text: label,
				collapsed: !EXPANDED.has(name),
				items: sub,
			});
		} else {
			// 容器目录：递归扫描子模块
			const subEntries = fs.readdirSync(dir, { withFileTypes: true });
			const childModules: any[] = [];

			for (const subEntry of subEntries) {
				if (!subEntry.isDirectory()) continue;
				const subName = subEntry.name;
				const subDir = path.join(dir, subName);
				if (!isModuleDir(subDir)) continue;

				const label = extractLabel(subDir);
				if (!label) continue;
				const sub = buildModuleItems(subDir, `spec/${name}/${subName}`);
				childModules.push({
					text: label,
					collapsed: true,
					items: sub,
				});
			}

			if (childModules.length > 0) {
				items.push({
					text: capitalize(name),
					collapsed: true,
					items: childModules,
				});
			}
		}
	}

	// 变更时间线置底
	if (fs.existsSync(path.join(specDir, "CHANGELOG.md"))) {
		items.push({ text: "变更时间线", link: "/spec/CHANGELOG" });
	}

	return items;
}