package com.copysync.android.data

import android.content.Context
import androidx.room.Dao
import androidx.room.Database
import androidx.room.Entity
import androidx.room.Index
import androidx.room.Insert
import androidx.room.OnConflictStrategy
import androidx.room.PrimaryKey
import androidx.room.Query
import androidx.room.Room
import androidx.room.RoomDatabase
import kotlinx.coroutines.flow.Flow

/** A clipboard history row (offline preservation + search). */
@Entity(tableName = "clips", indices = [Index("sha"), Index("ts")])
data class ClipEntity(
    @PrimaryKey(autoGenerate = true) val rowid: Long = 0,
    val clipId: String,
    val ts: Long,
    val direction: String, // "in" | "out"
    val origin: String,
    val text: String,
    val sha: String,
    val kind: String = "text", // text | image | file
    val blobId: String = "",
    val name: String = "",
    val sizeBytes: Long = 0,
    val mime: String = "",
    val enc: Boolean = false, // payload was E2E-encrypted on the wire
    val localPath: String? = null, // set after a file is downloaded
)

@Dao
interface ClipDao {
    @Insert(onConflict = OnConflictStrategy.IGNORE)
    suspend fun insert(e: ClipEntity)

    @Query("SELECT * FROM clips ORDER BY ts DESC LIMIT 300")
    fun recent(): Flow<List<ClipEntity>>

    @Query("SELECT * FROM clips WHERE text LIKE '%' || :q || '%' ORDER BY ts DESC LIMIT 300")
    fun search(q: String): Flow<List<ClipEntity>>

    @Query("UPDATE clips SET localPath = :path WHERE rowid = :rowid")
    suspend fun setLocalPath(rowid: Long, path: String)

    @Query("DELETE FROM clips WHERE rowid NOT IN (SELECT rowid FROM clips ORDER BY ts DESC LIMIT :keep)")
    suspend fun prune(keep: Int)
}

@Database(entities = [ClipEntity::class], version = 3, exportSchema = false)
abstract class HistoryDb : RoomDatabase() {
    abstract fun clipDao(): ClipDao

    companion object {
        @Volatile
        private var instance: HistoryDb? = null

        fun get(context: Context): HistoryDb = instance ?: synchronized(this) {
            instance ?: Room.databaseBuilder(
                context.applicationContext, HistoryDb::class.java, "copysync-history.db",
            ).fallbackToDestructiveMigration().build().also { instance = it }
        }
    }
}
