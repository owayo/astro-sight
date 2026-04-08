package com.example

import kotlin.collections.mutableListOf

class SampleRepository {
    private val items = mutableListOf<String>()

    fun add(item: String) {
        items.add(item)
    }

    fun findAll(): List<String> = items.toList()

    fun count(): Int = items.size
}

fun main() {
    val repo = SampleRepository()
    repo.add("hello")
    println(repo.findAll())
}
