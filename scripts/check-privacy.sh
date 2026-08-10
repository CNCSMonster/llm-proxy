#!/bin/bash
# check-privacy.sh — 隐私检查脚本
# 检查 staged 文件或整个工作区中的隐私信息泄露
#
# 用法：
#   scripts/check-privacy.sh              # 检查 staged 文件
#   scripts/check-privacy.sh --all        # 检查整个工作区
#   scripts/check-privacy.sh --history    # 检查 git 历史
#
# 退出码：
#   0 = 通过
#   1 = 发现隐私问题

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

MODE="${1:---staged}"
ISSUES=0

echo "=== 隐私检查 ==="
echo "模式: $MODE"
echo ""

# Token 模式（排除 EXAMPLE 占位符）
TOKEN_PATTERNS=(
    'ya29\.a0[A-Za-z0-9_-]{20,}'
    '1//0[A-Za-z0-9_-]{20,}'
    'rt\.1\.[A-Za-z0-9_-]{20,}'
    'sk-[a-zA-Z0-9]{20,}'
    'tp-[a-zA-Z0-9]{20,}'
    'ghp_[A-Za-z0-9]{20,}'
    'github_pat_[A-Za-z0-9]{20,}'
)

# 内部标识
INTERNAL_PATTERNS=(
    'tencent-cloud-server'
    'my-git-ssh'
)

# 敏感文件名（排除 node_modules/vendor/target 等）
SENSITIVE_FILES=(
    'oauth_accounts\.json'
    '\.key$'
    '\.pem$'
    '\.p12$'
    '\.pfx$'
    'ENV-KEYS\.md'
)

check_content() {
    local files="$1"
    local label="$2"

    if [ -z "$files" ]; then
        return
    fi

    # 检查 token
    for pattern in "${TOKEN_PATTERNS[@]}"; do
        matches=$(echo "$files" | xargs grep -n -E "$pattern" 2>/dev/null | grep -v "EXAMPLE" || true)
        if [ -n "$matches" ]; then
            echo -e "${RED}❌ 发现疑似真实 token ($label):${NC}"
            echo "$matches" | head -5
            ISSUES=$((ISSUES + 1))
        fi
    done

    # 检查内部标识
    for pattern in "${INTERNAL_PATTERNS[@]}"; do
        matches=$(echo "$files" | xargs grep -n "$pattern" 2>/dev/null || true)
        if [ -n "$matches" ]; then
            echo -e "${YELLOW}⚠️  发现内部标识 ($label):${NC}"
            echo "$matches" | head -5
            ISSUES=$((ISSUES + 1))
        fi
    done
}

check_sensitive_files() {
    local files="$1"

    for pattern in "${SENSITIVE_FILES[@]}"; do
        matches=$(echo "$files" | grep -v "node_modules\|vendor\|target\|\.git" | grep -E "$pattern" || true)
        if [ -n "$matches" ]; then
            echo -e "${RED}❌ 发现敏感文件:${NC}"
            echo "$matches"
            ISSUES=$((ISSUES + 1))
        fi
    done
}

case "$MODE" in
    --staged)
        echo "检查 staged 文件..."
        staged_files=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null || true)
        check_content "$staged_files" "staged"
        check_sensitive_files "$staged_files"
        ;;
    --all)
        echo "检查工作区所有文件..."
        all_files=$(find . -type f \( -name "*.md" -o -name "*.rs" -o -name "*.toml" -o -name "*.json" \) -not -path "./.git/*" -not -path "./target/*" -not -path "*/node_modules/*" -not -path "*/vendor/*" 2>/dev/null || true)
        check_content "$all_files" "workspace"
        check_sensitive_files "$all_files"
        ;;
    --history)
        echo "检查 git 历史..."
        for pattern in "${TOKEN_PATTERNS[@]}"; do
            matches=$(git log --all -p 2>/dev/null | grep -E "$pattern" | grep -v "EXAMPLE" | head -5 || true)
            if [ -n "$matches" ]; then
                echo -e "${RED}❌ git 历史中发现疑似真实 token:${NC}"
                echo "$matches"
                ISSUES=$((ISSUES + 1))
            fi
        done
        for pattern in "${INTERNAL_PATTERNS[@]}"; do
            # 排除 commit author 信息
            matches=$(git log --all -p 2>/dev/null | grep -v "^Author:" | grep -v "^Commit:" | grep "$pattern" | head -5 || true)
            if [ -n "$matches" ]; then
                echo -e "${YELLOW}⚠️  git 历史中发现内部标识:${NC}"
                echo "$matches"
                ISSUES=$((ISSUES + 1))
            fi
        done
        ;;
    *)
        echo "用法: $0 [--staged|--all|--history]"
        exit 1
        ;;
esac

echo ""
if [ $ISSUES -eq 0 ]; then
    echo -e "${GREEN}✅ 隐私检查通过${NC}"
    exit 0
else
    echo -e "${RED}❌ 发现 $ISSUES 个隐私问题，请修复后再提交${NC}"
    exit 1
fi
