<script setup lang="ts">
import { useRouter } from 'vue-router'
import PageHeader from '../components/PageHeader.vue'

const router = useRouter()
</script>

<template>
  <div class="page">
    <PageHeader title="页面不存在" />
    <el-result icon="error" title="404" sub-title="路径无效：请从左侧导航进入，或使用带页面编号的深链（docs/13 §3.1）。">
      <template #extra>
        <el-button type="primary" @click="router.push('/')">返回总览</el-button>
      </template>
    </el-result>
  </div>
</template>
