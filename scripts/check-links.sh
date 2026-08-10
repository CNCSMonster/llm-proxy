#!/bin/bash
# check-links.sh — 检查文档中的悬空链接
#
# 用法：
#   scripts/check-links.sh              # 检查当前目录
#   scripts/check-links.sh <directory>  # 检查指定目录
#
# 退出码：
#   0 = 通过
#   1 = 发现悬空链接

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

DIR="${1:-.}"
ISSUES=0

echo "=== 链接检查 ==="
echo "目录: $DIR"
echo ""

# 检查引用不存在目录的链接
echo "检查悬空链接..."

# 检查 docs/research/ 引用（公开仓库不存在此目录）
matches=$(grep -rn "docs/research\|../research" "$DIR" --include="*.md" 2>/dev/null | grep -v "archived/" | grep -v "内部" || true)
if [ -n "$matches" ]; then
    echo -e "${YELLOW}⚠️  发现 docs/research/ 引用（公开仓库不存在此目录）:${NC}"
    echo "$matches" | head -10
    ISSUES=$((ISSUES + 1))
fi

# 检查 docs/plans/ 引用（公开仓库不存在此目录）
matches=$(grep -rn "docs/plans\|../plans" "$DIR" --include="*.md" 2>/dev/null | grep -v "内部" || true)
if [ -n "$matches" ]; then
    echo -e "${YELLOW}⚠️  发现 docs/plans/ 引用（公开仓库不存在此目录）:${NC}"
    echo "$matches" | head -10
    ISSUES=$((ISSUES + 1))
fi

# 检查 docs/reports/ 引用（公开仓库不存在此目录）
matches=$(grep -rn "docs/reports\|../reports" "$DIR" --include="*.md" 2>/dev/null | grep -v "内部" || true)
if [ -n "$matches" ]; then
    echo -e "${YELLOW}⚠️  发现 docs/reports/ 引用（公开仓库不存在此目录）:${NC}"
    echo "$matches" | head -10
    ISSUES=$((ISSUES + 1))
fi

# 检查相对链接是否指向存在的文件
echo ""
echo "检查相对链接目标..."

# 提取所有相对链接
links=$(grep -roh '\[.*\]\((\.\./[^)]*\|[^)]*/[^)]*)\)' "$DIR" --include="*.md" 2>/dev/null | grep -oE '\((\.\./[^)]*\|[^)]*/[^)]*)\)' | tr -d '()' | sort -u || true)

for link in $links; do
    # 跳过外部链接
    if [[ "$link" == http* ]]; then
        continue
    fi
    
    # 检查文件是否存在
    if [ ! -f "$DIR/$link" ] && [ ! -d "$DIR/$link" ]; then
        echo -e "${RED}❌ 悬空链接: $link${NC}"
        # 显示引用此链接的文件
        grep -rn "$link" "$DIR" --include="*.md" 2>/dev/null | head -3
        ISSUES=$((ISSUES + 1))
    fi
done

echo ""
if [ $ISSUES -eq 0 ]; then
    echo -e "${GREEN}✅ 链接检查通过${NC}"
    exit 0
else
    echo -e "${RED}❌ 发现 $ISSUES 个链接问题${NC}"
    exit 1
fi
